#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "the standalone integration test uses assertion panics and explicit fixture expectations"
)]

use signalbox_persistence::test_support::postgres::TestDatabase;
use std::{error::Error, num::NonZeroU64, time::Duration};

use rust_decimal::Decimal;
use sha2::{Digest, Sha256};
use signalbox_domain::{
    DirectModelSelection, ModelSelectionRequest, ModuleDispatch, SessionConfigurationDefaults,
    SessionCreationCause, SessionCreationProvenance,
};
use signalbox_module_repo_watch_v2::{
    CreateSessionCommandFactory, DispatchAdmission, DispatchReferenceGenerator, EventAdmission,
    EventCandidate, EventProducer, FrontierEventAdmission, FrontierReleaseAdmission,
    LifecycleReactionError, PullRequestLifecycle, PullRequestState, RepoWatchStore,
    RepositoryProjection, RepositoryRuleSet, RepositoryState, RuleAdmission,
    RuleReconciliationAdmission, SessionCommandCodec, StoreError, WebhookAdmission,
    WebhookDelivery, WebhookDisposition, matching_rules, plan_lifecycle_reaction_for_test,
    plan_repository_event, plan_retained_lifecycle_reaction_for_test,
};
use signalbox_persistence::{
    disposable_postgres_server_args, disposable_postgres_state_tmpfs_from_example,
    disposable_test_container_labels, local_test_connection_options, migrate,
};
use signalbox_session_ownership::{
    BranchName, CheckConclusion, CheckRunName, ChecksOutcome, CommitSha, CreateSession,
    CreateSessionOutcome, DescendantTerminationScope, DurableCommandId, FinishCondition,
    GitHubObjectId, LabelName, LifecycleEvent, MergeableState, OffsetDateTime, PullRequestBody,
    PullRequestEventContext, PullRequestEventContextInput, PullRequestNumber, PullRequestTitle,
    ReactionContent, ReactionSubject, RepoWatchAuthorLogin, RepoWatchBranchHead,
    RepoWatchCheckCompletionGeneration, RepoWatchCheckRunObservation,
    RepoWatchCheckSuiteObservation, RepoWatchDispatchId, RepoWatchEvent,
    RepoWatchEventContentIdentityV1, RepoWatchEventId, RepoWatchEventIdentityFrontierEntryV1,
    RepoWatchEventIdentityFrontierV1, RepoWatchEventKindNameV1, RepoWatchLabelMatcher,
    RepoWatchMatcherV1, RepoWatchMatcherV1Input, RepoWatchObservation,
    RepoWatchPullRequestLifecycle, RepoWatchPullRequestState as ComparisonPullRequestState,
    RepoWatchPullRequestStateInput, RepoWatchReactionObservation, RepoWatchRepositoryState,
    RepoWatchRepositoryStateInput, RepoWatchReviewObservation, RepoWatchRule,
    RepoWatchRuleActionV1, RepoWatchRuleId, RepoWatchRuleVersion, RepoWatchSingletonScope,
    RepoWatchThreadObservation, RepoWatchThreadState, RepoWatchWorkflowRunAttempt,
    RepoWatchWorkflowRunObservation, RepositorySlug, ReviewState, ReviewThreadId, SessionCommand,
    SessionCommandPayload, SessionCreated, SessionId, SessionLifecycleCommand,
    SessionLifecycleOperation, SessionOwnership, SessionTemplateName, StartGate, StopStickiness,
    WorkflowName,
};
use sqlx::{PgPool, postgres::PgPoolOptions};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
};
use uuid::Uuid;

#[path = "repo_watch_v2/checkout.rs"]
mod checkout;
#[path = "repo_watch_v2/provider_identity.rs"]
mod provider_identity;
#[path = "repo_watch_v2/retirement.rs"]
mod retirement;

// The configured merged-subject retention window is seven days.
const MERGED_RETENTION: Duration = Duration::from_secs(604_800);

const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";

fn event_candidate<'a>(
    event: &'a RepoWatchEvent,
    content_identity: RepoWatchEventContentIdentityV1,
) -> EventCandidate<'a> {
    EventCandidate {
        event,
        content_identity,
    }
}

fn frontier_entries(
    frontier: &RepoWatchEventIdentityFrontierV1,
) -> Vec<RepoWatchEventIdentityFrontierEntryV1> {
    frontier.entries().collect()
}
const DATABASE_NAME: &str = "signalbox_repo_watch_v2";
const DATABASE_USER: &str = "signalbox";
const DATABASE_PASSWORD: &str = "signalbox-test-only";

struct FixedDispatchIds {
    value: u128,
    calls: usize,
}

impl DispatchReferenceGenerator for FixedDispatchIds {
    fn next_dispatch(&mut self) -> RepoWatchDispatchId {
        let value = self.value + self.calls as u128;
        self.calls += 1;
        RepoWatchDispatchId::from_uuid(Uuid::from_u128(value))
    }
}

struct FixtureSessionFactory {
    next_command: u128,
    model: u128,
}

impl CreateSessionCommandFactory for FixtureSessionFactory {
    type Error = std::convert::Infallible;

    fn create_session(
        &mut self,
        dispatch: RepoWatchDispatchId,
        _template: &SessionTemplateName,
        _event: &RepoWatchEvent,
    ) -> Result<CreateSession, Self::Error> {
        let command = DurableCommandId::from_uuid(Uuid::from_u128(self.next_command));
        self.next_command += 1;
        Ok(CreateSession::new(
            command,
            SessionCreationProvenance::module_dispatched(ModuleDispatch::RepositoryWatch {
                dispatch,
            }),
            SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(
                DirectModelSelection::from_uuid(Uuid::from_u128(self.model)),
            )),
        ))
    }
}

struct FixtureCommandCodec;

impl SessionCommandCodec for FixtureCommandCodec {
    fn encode(&mut self, command: &SessionCommand) -> Option<Vec<u8>> {
        match command.clone().into_payload() {
            SessionCommandPayload::CreateSession(command) => {
                let SessionCreationCause::ModuleDispatched {
                    dispatch: ModuleDispatch::RepositoryWatch { dispatch },
                } = command.provenance().cause()
                else {
                    return None;
                };
                let ModelSelectionRequest::Direct(model) =
                    command.initial_configuration_defaults().model()
                else {
                    return None;
                };
                let mut encoded = Vec::with_capacity(49);
                encoded.push(1);
                encoded.extend_from_slice(command.command_id().as_uuid().as_bytes());
                encoded.extend_from_slice(dispatch.as_uuid().as_bytes());
                encoded.extend_from_slice(model.as_uuid().as_bytes());
                Some(encoded)
            }
            SessionCommandPayload::Lifecycle(command) => {
                let operation = match command.operation() {
                    SessionLifecycleOperation::ReleaseStart => 0,
                    SessionLifecycleOperation::Stop {
                        sticky: StopStickiness::Sticky,
                        descendant_scope: DescendantTerminationScope::ParentAlone,
                    } => 1,
                    SessionLifecycleOperation::Stop {
                        sticky: StopStickiness::Sticky,
                        descendant_scope: DescendantTerminationScope::ParentAndDescendants,
                    } => 2,
                    _ => return None,
                };
                let mut encoded = Vec::with_capacity(34);
                encoded.push(2);
                encoded.extend_from_slice(command.command_id().as_uuid().as_bytes());
                encoded.extend_from_slice(command.session().as_uuid().as_bytes());
                encoded.push(operation);
                Some(encoded)
            }
            SessionCommandPayload::SubmitInput(_) | SessionCommandPayload::Goal(_) => None,
        }
    }

    fn decode(&mut self, payload: &[u8]) -> Option<SessionCommand> {
        let uuid = |start| {
            let bytes: [u8; 16] = payload.get(start..start + 16)?.try_into().ok()?;
            Some(Uuid::from_bytes(bytes))
        };
        match payload.first().copied()? {
            1 if payload.len() == 49 => {
                let command = DurableCommandId::from_uuid(uuid(1)?);
                let dispatch = RepoWatchDispatchId::from_uuid(uuid(17)?);
                let model = DirectModelSelection::from_uuid(uuid(33)?);
                SessionCommand::create_session(
                    CreateSession::new(
                        command,
                        SessionCreationProvenance::module_dispatched(
                            ModuleDispatch::RepositoryWatch { dispatch },
                        ),
                        SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(model)),
                    )
                    .with_lifecycle(
                        StartGate::Held,
                        SessionOwnership::Owned,
                        Some(FinishCondition::ExternalGate),
                    ),
                )
                .ok()
            }
            2 if payload.len() == 34 => {
                let command = DurableCommandId::from_uuid(uuid(1)?);
                let session = SessionId::from_uuid(uuid(17)?);
                let operation = match payload[33] {
                    0 => SessionLifecycleOperation::ReleaseStart,
                    1 => SessionLifecycleOperation::Stop {
                        sticky: StopStickiness::Sticky,
                        descendant_scope: DescendantTerminationScope::ParentAlone,
                    },
                    2 => SessionLifecycleOperation::Stop {
                        sticky: StopStickiness::Sticky,
                        descendant_scope: DescendantTerminationScope::ParentAndDescendants,
                    },
                    _ => return None,
                };
                SessionCommand::lifecycle(SessionLifecycleCommand::new(command, session, operation))
                    .ok()
            }
            _ => None,
        }
    }
}

struct DecodeOnlyCommandCodec;

impl SessionCommandCodec for DecodeOnlyCommandCodec {
    fn encode(&mut self, _command: &SessionCommand) -> Option<Vec<u8>> {
        None
    }

    fn decode(&mut self, payload: &[u8]) -> Option<SessionCommand> {
        FixtureCommandCodec.decode(payload)
    }
}

async fn postgres() -> Result<(TestDatabase, PgPool, String), Box<dyn Error>> {
    signalbox_persistence::test_support::postgres::migrated_postgres(4).await
}

/// Uses a dedicated server for cluster-wide state and historical migrations.
async fn unmigrated_postgres() -> Result<(ContainerAsync<Postgres>, PgPool, String), Box<dyn Error>>
{
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
    Ok((container, pool, database_url))
}

async fn module_pool(database_url: &str) -> Result<PgPool, sqlx::Error> {
    let options = local_test_connection_options(database_url)?.username("mod_repo_watch");
    PgPoolOptions::new()
        .max_connections(2)
        .after_connect(|connection, _metadata| {
            Box::pin(async move {
                sqlx::query("SET search_path = mod_repo_watch, pg_catalog")
                    .execute(&mut *connection)
                    .await?;
                Ok(())
            })
        })
        .connect_with(options)
        .await
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn v2_ingest_is_idempotent_under_the_module_role() -> Result<(), Box<dyn Error>> {
    let (container, core_pool, database_url) = postgres().await?;
    migrate(&core_pool).await?;
    sqlx::query("ALTER ROLE mod_repo_watch PASSWORD 'signalbox-test-only'")
        .execute(&core_pool)
        .await?;
    let module_pool = module_pool(&database_url).await?;
    let store = RepoWatchStore::new(module_pool.clone());

    let repository = RepositorySlug::try_new(String::from("owner/repository"))?;
    let default_branch = BranchName::try_new(String::from("main"))?;
    let default_head =
        CommitSha::try_new(String::from("1111111111111111111111111111111111111111"))?;
    let observed_at =
        OffsetDateTime::UNIX_EPOCH + Duration::from_secs(1_000) + Duration::from_nanos(123);
    let body = br#"{"action":"opened"}"#;
    let delivery = || WebhookDelivery {
        repository: &repository,
        hook_id: 9,
        delivery_id: Uuid::from_u128(10),
        event: "pull_request",
        action: Some("opened"),
        body,
        received_at: observed_at,
        expires_at: observed_at + Duration::from_secs(60),
    };
    assert_eq!(
        store.admit_webhook(delivery()).await?,
        WebhookAdmission::Inserted
    );
    assert_eq!(
        store
            .ingestion_measurements(&repository)
            .last_accepted_webhook,
        Some(observed_at)
    );
    let mut replay = delivery();
    replay.received_at += Duration::from_secs(1);
    assert_eq!(
        store.admit_webhook(replay).await?,
        WebhookAdmission::PendingReplay
    );
    assert_eq!(
        store
            .ingestion_measurements(&repository)
            .last_accepted_webhook,
        Some(observed_at),
        "delivery replays preserve the accepted high-water mark"
    );
    let stored_digest: Vec<u8> = sqlx::query_scalar(
        "SELECT body_digest
           FROM webhook_delivery
          WHERE hook_id = 9 AND delivery_id = $1",
    )
    .bind(Uuid::from_u128(10))
    .fetch_one(&module_pool)
    .await?;
    assert_eq!(stored_digest, Sha256::digest(body).as_slice());

    let repository_state = RepositoryState {
        repository: &repository,
        default_branch: &default_branch,
        default_head: &default_head,
        observed_at,
    };

    let title = PullRequestTitle::try_new(String::from("A bounded rewrite"))?;
    let body = PullRequestBody::try_new(String::from("Current provider state"))?;
    let author = RepoWatchAuthorLogin::try_new(String::from("octocat"))?;
    let label = LabelName::try_new(String::from("ready"))?;
    let comparison_pull_request =
        ComparisonPullRequestState::try_new(RepoWatchPullRequestStateInput {
            context: PullRequestEventContext::new(PullRequestEventContextInput {
                number: PullRequestNumber::new(NonZeroU64::new(7).expect("seven is positive")),
                head_sha: default_head.clone(),
                head_repository: repository.clone(),
                base_branch: default_branch.clone(),
                head_branch: default_branch.clone(),
                title: title.clone(),
                body: body.clone(),
                labels: vec![label],
                draft: false,
                author: Some(author.clone()),
            }),
            lifecycle: RepoWatchPullRequestLifecycle::Open,
            mergeable_state: MergeableState::Mergeable,
            completed_check_suites: vec![RepoWatchCheckSuiteObservation::new(
                GitHubObjectId::new(NonZeroU64::new(20).expect("twenty is positive")),
                RepoWatchCheckCompletionGeneration::try_new(String::from("suite-1"))?,
                ChecksOutcome::Success,
            )],
            completed_check_runs: vec![RepoWatchCheckRunObservation::new(
                GitHubObjectId::new(NonZeroU64::new(21).expect("twenty-one is positive")),
                RepoWatchCheckCompletionGeneration::try_new(String::from("run-1"))?,
                CheckRunName::try_new(String::from("test"))?,
                CheckConclusion::Success,
            )],
            reviews: vec![RepoWatchReviewObservation::new(
                GitHubObjectId::new(NonZeroU64::new(22).expect("twenty-two is positive")),
                author.clone(),
                Some(ReviewState::Approved),
                default_head.clone(),
            )],
            threads: vec![RepoWatchThreadObservation::new(
                ReviewThreadId::try_new(String::from("thread-1"))?,
                RepoWatchThreadState::Resolved,
            )],
            reactions: vec![RepoWatchReactionObservation::new(
                ReactionSubject::ReviewComment {
                    id: GitHubObjectId::new(NonZeroU64::new(23).expect("twenty-three is positive")),
                },
                author.clone(),
                ReactionContent::try_new(String::from("+1"))?,
            )],
        })?;
    let comparison_baseline = RepoWatchObservation::new(
        vec![author.clone()],
        RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput {
            pull_requests: vec![comparison_pull_request],
            workflow_runs: vec![RepoWatchWorkflowRunObservation::new(
                GitHubObjectId::new(NonZeroU64::new(24).expect("twenty-four is positive")),
                GitHubObjectId::new(NonZeroU64::new(25).expect("twenty-five is positive")),
                RepoWatchWorkflowRunAttempt::new(NonZeroU64::new(2).expect("two is positive")),
                default_branch.clone(),
                WorkflowName::try_new(String::from("ci"))?,
                CheckConclusion::Success,
            )],
            branch_heads: vec![RepoWatchBranchHead::new(
                default_branch.clone(),
                default_head.clone(),
            )],
        })?,
    );
    let pull_request_state = PullRequestState {
        repository: &repository,
        number: PullRequestNumber::new(NonZeroU64::new(7).expect("seven is positive")),
        lifecycle: PullRequestLifecycle::Open,
        head: &default_head,
        head_repository: &repository,
        head_branch: &default_branch,
        base_branch: &default_branch,
        title: &title,
        body: &body,
        draft: false,
        author: Some(&author),
        observed_at,
    };
    let mut projection = RepositoryProjection {
        repository: repository_state,
        pull_requests: vec![pull_request_state],
        comparison_baseline: &comparison_baseline,
        merged_baselines: &[],
    };

    let stream = [13; 32];
    let frontier_entry = RepoWatchEventIdentityFrontierEntryV1::for_pull_request(
        stream,
        NonZeroU64::new(2).expect("two is positive"),
        PullRequestNumber::new(NonZeroU64::new(7).expect("seven is positive")),
    );
    let frontier = RepoWatchEventIdentityFrontierV1::try_from_entries(vec![frontier_entry])?;
    let rule = RepoWatchRule::try_new(
        RepoWatchRuleId::try_new(String::from("branch-ci"))?,
        RepoWatchRuleVersion::V1,
        RepoWatchMatcherV1::new(RepoWatchMatcherV1Input {
            event_kinds: vec![RepoWatchEventKindNameV1::BranchWorkflowRunCompleted],
            repository: Some(repository.clone()),
            labels: RepoWatchLabelMatcher::default(),
            ..RepoWatchMatcherV1Input::default()
        }),
        vec![
            RepoWatchRuleActionV1::DispatchSession {
                template: SessionTemplateName::try_new(String::from("repo-watch"))?,
            },
            RepoWatchRuleActionV1::DispatchSession {
                template: SessionTemplateName::try_new(String::from("repo-watch-followup"))?,
            },
        ],
        RepoWatchSingletonScope::Repository,
        Duration::ZERO,
    )?;
    let second_rule = RepoWatchRule::try_new(
        RepoWatchRuleId::try_new(String::from("branch-ci-second"))?,
        RepoWatchRuleVersion::V1,
        RepoWatchMatcherV1::new(RepoWatchMatcherV1Input {
            event_kinds: vec![RepoWatchEventKindNameV1::BranchWorkflowRunCompleted],
            repository: Some(repository.clone()),
            labels: RepoWatchLabelMatcher::default(),
            ..RepoWatchMatcherV1Input::default()
        }),
        vec![RepoWatchRuleActionV1::DispatchSession {
            template: SessionTemplateName::try_new(String::from("repo-watch"))?,
        }],
        RepoWatchSingletonScope::Repository,
        Duration::ZERO,
    )?;
    let first_configuration = [RepositoryRuleSet::new(
        &repository,
        std::slice::from_ref(&rule),
    )];
    let concurrent_configuration = [RepositoryRuleSet::new(
        &repository,
        std::slice::from_ref(&rule),
    )];
    let (first, concurrent) = tokio::join!(
        store.reconcile_rules(&first_configuration, observed_at),
        store.reconcile_rules(&concurrent_configuration, observed_at)
    );
    assert!(matches!(
        (first?, concurrent?),
        (
            RuleReconciliationAdmission::Applied { rules: first, deactivated: 0 },
            RuleReconciliationAdmission::Applied { rules: second, deactivated: 0 }
        ) if (first.as_ref() == [RuleAdmission::Inserted]
            && second.as_ref() == [RuleAdmission::Replayed])
            || (first.as_ref() == [RuleAdmission::Replayed]
                && second.as_ref() == [RuleAdmission::Inserted])
    ));
    assert_eq!(
        store
            .reconcile_rules(
                &[RepositoryRuleSet::new(
                    &repository,
                    std::slice::from_ref(&rule),
                )],
                observed_at,
            )
            .await?,
        RuleReconciliationAdmission::Applied {
            rules: Box::new([RuleAdmission::Replayed]),
            deactivated: 0,
        }
    );
    let fingerprint_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM rule_field_fingerprint
          WHERE repository = $1 AND rule_id = $2",
    )
    .bind(repository.as_str())
    .bind(rule.id().as_str())
    .fetch_one(&module_pool)
    .await?;
    assert_eq!(
        usize::try_from(fingerprint_count)?,
        rule.identity_field_digests().len()
    );
    let other_repository = RepositorySlug::try_new(String::from("other/repository"))?;
    assert_eq!(
        store
            .reconcile_rules(
                &[
                    RepositoryRuleSet::new(&repository, std::slice::from_ref(&rule)),
                    RepositoryRuleSet::new(&other_repository, std::slice::from_ref(&rule),),
                ],
                observed_at,
            )
            .await?,
        RuleReconciliationAdmission::Applied {
            rules: Box::new([RuleAdmission::Replayed, RuleAdmission::Inserted]),
            deactivated: 0,
        }
    );
    assert_eq!(
        store
            .reconcile_rules(
                &[
                    RepositoryRuleSet::new(&repository, std::slice::from_ref(&rule)),
                    RepositoryRuleSet::new(&other_repository, &[]),
                ],
                observed_at,
            )
            .await?,
        RuleReconciliationAdmission::Applied {
            rules: Box::new([RuleAdmission::Replayed]),
            deactivated: 1,
        }
    );
    let retained_revisions: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM rule_revision
          WHERE repository = $1 AND rule_id = $2",
    )
    .bind(other_repository.as_str())
    .bind(rule.id().as_str())
    .fetch_one(&module_pool)
    .await?;
    assert_eq!(retained_revisions, 1);
    assert_eq!(
        store
            .reconcile_rules(
                &[
                    RepositoryRuleSet::new(
                        &repository,
                        &[rule.clone(), second_rule.clone()],
                    ),
                    RepositoryRuleSet::new(
                        &other_repository,
                        &[second_rule.clone(), rule.clone()],
                    ),
                ],
                observed_at,
            )
            .await?,
        RuleReconciliationAdmission::Stale
    );
    let partially_inserted_rules: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM rule_revision
          WHERE repository = $1 AND rule_id = $2",
    )
    .bind(other_repository.as_str())
    .bind(second_rule.id().as_str())
    .fetch_one(&module_pool)
    .await?;
    assert_eq!(partially_inserted_rules, 0);
    let partially_inserted_main_rules: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM rule_revision
          WHERE repository = $1 AND rule_id = $2",
    )
    .bind(repository.as_str())
    .bind(second_rule.id().as_str())
    .fetch_one(&module_pool)
    .await?;
    assert_eq!(partially_inserted_main_rules, 0);

    let event = RepoWatchEvent::branch_workflow(
        RepoWatchEventId::from_uuid(Uuid::from_u128(14)),
        repository.clone(),
        default_branch.clone(),
        WorkflowName::try_new(String::from("ci"))?,
        signalbox_session_ownership::CheckConclusion::Success,
    );
    assert_eq!(matching_rules(std::slice::from_ref(&rule), &event), [&rule]);
    let identity = RepoWatchEventContentIdentityV1::from_bytes([15; 32]);
    let occurrence = event_candidate(&event, identity);
    let earlier_event = RepoWatchEvent::branch_workflow(
        RepoWatchEventId::from_uuid(Uuid::from_u128(13)),
        repository.clone(),
        default_branch.clone(),
        WorkflowName::try_new(String::from("build"))?,
        signalbox_session_ownership::CheckConclusion::Success,
    );
    let earlier_identity = RepoWatchEventContentIdentityV1::from_bytes([14; 32]);
    let earlier_occurrence = event_candidate(&earlier_event, earlier_identity);
    assert_eq!(
        store
            .commit_frontier_candidate(
                &projection,
                0,
                &frontier_entries(&frontier),
                &[earlier_occurrence, occurrence],
                EventProducer::Poll,
                observed_at,
            )
            .await?,
        FrontierEventAdmission::Committed {
            generation: 1,
            events: Box::new([EventAdmission::Inserted, EventAdmission::Inserted]),
        }
    );
    let evaluation_order: Vec<(Uuid, Decimal, Decimal, Decimal)> = sqlx::query_as(
        "SELECT event_id, repository_event_ordinal, frontier_generation, event_ordinal
           FROM gh_event WHERE repository = $1 ORDER BY repository_event_ordinal",
    )
    .bind(repository.as_str())
    .fetch_all(&module_pool)
    .await?;
    assert_eq!(
        evaluation_order,
        vec![
            (
                earlier_event.id().into_uuid(),
                Decimal::from(1_u64),
                Decimal::from(1_u64),
                Decimal::from(1_u64),
            ),
            (
                event.id().into_uuid(),
                Decimal::from(2_u64),
                Decimal::from(1_u64),
                Decimal::from(2_u64),
            ),
        ]
    );
    let loaded = RepoWatchStore::new(module_pool.clone())
        .ingest_baseline(&repository)
        .await?;
    assert_eq!(loaded.generation, 1);
    assert_eq!(loaded.observation.as_ref(), Some(&comparison_baseline));
    assert_eq!(loaded.frontier, frontier);
    let retained_event_source: (String, Decimal) = sqlx::query_as(
        "SELECT producer, repository_event_ordinal FROM gh_event WHERE event_id = $1",
    )
    .bind(event.id().into_uuid())
    .fetch_one(&module_pool)
    .await?;
    assert_eq!(
        retained_event_source,
        (String::from("poll"), Decimal::from(2_u64))
    );
    let retention_column_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM information_schema.columns
          WHERE table_schema = 'mod_repo_watch'
            AND table_name = 'gh_event'
            AND column_name = 'retain_until'",
    )
    .fetch_one(&module_pool)
    .await?;
    assert_eq!(retention_column_count, 0);
    assert_eq!(
        store
            .reconcile_rules(
                &[RepositoryRuleSet::new(
                    &repository,
                    &[rule.clone(), second_rule.clone()],
                )],
                observed_at + Duration::from_secs(1),
            )
            .await?,
        RuleReconciliationAdmission::Applied {
            rules: Box::new([RuleAdmission::Replayed, RuleAdmission::Inserted]),
            deactivated: 0,
        }
    );
    let activation_tail: Decimal = sqlx::query_scalar(
        "SELECT activated_after_event_ordinal FROM rule_revision
          WHERE repository = $1 AND rule_id = $2 AND revision = 1",
    )
    .bind(repository.as_str())
    .bind(second_rule.id().as_str())
    .fetch_one(&module_pool)
    .await?;
    assert_eq!(activation_tail, Decimal::from(2_u64));
    let late_rule = RepoWatchRule::try_new(
        RepoWatchRuleId::try_new(String::from("late-branch-ci"))?,
        RepoWatchRuleVersion::V1,
        RepoWatchMatcherV1::new(RepoWatchMatcherV1Input {
            event_kinds: vec![RepoWatchEventKindNameV1::BranchWorkflowRunCompleted],
            repository: Some(repository.clone()),
            labels: RepoWatchLabelMatcher::default(),
            ..RepoWatchMatcherV1Input::default()
        }),
        vec![RepoWatchRuleActionV1::DispatchSession {
            template: SessionTemplateName::try_new(String::from("late-repo-watch"))?,
        }],
        RepoWatchSingletonScope::Repository,
        Duration::ZERO,
    )?;
    assert_eq!(
        store
            .reconcile_rules(
                &[RepositoryRuleSet::new(
                    &repository,
                    &[rule.clone(), second_rule.clone(), late_rule.clone()],
                )],
                observed_at,
            )
            .await?,
        RuleReconciliationAdmission::Applied {
            rules: Box::new([
                RuleAdmission::Replayed,
                RuleAdmission::Replayed,
                RuleAdmission::Inserted,
            ]),
            deactivated: 0,
        }
    );
    let mut late_ids = FixedDispatchIds {
        value: 140,
        calls: 0,
    };
    let mut late_factory = FixtureSessionFactory {
        next_command: 141,
        model: 18,
    };
    let late_batches = plan_repository_event(
        std::slice::from_ref(&late_rule),
        &event,
        &mut late_ids,
        &mut late_factory,
    )?;
    let mut command_codec = FixtureCommandCodec;
    assert!(matches!(
        store
            .record_commands(&late_batches[0], observed_at, &mut command_codec)
            .await?,
        DispatchAdmission::InactiveRule
    ));
    let complete_baseline_retained: bool = sqlx::query_scalar(
        "SELECT comparison_baseline @> $2::jsonb
           FROM repository_state WHERE repository = $1",
    )
    .bind(repository.as_str())
    .bind(
        r#"{
          "signal_reviewers":["octocat"],
          "pull_requests":[{
            "context":{"labels":["ready"]},
            "mergeable_state":"mergeable",
            "completed_check_suites":[{"completion_generation":"suite-1"}],
            "completed_check_runs":[{"completion_generation":"run-1"}],
            "reviews":[{"reviewer":"octocat","state":"approved"}],
            "threads":[{"thread":"thread-1","state":"resolved"}],
            "reactions":[{"reactor":"octocat","content":"+1"}]
          }],
          "workflow_runs":[{"workflow":"ci","attempt":2}],
          "branch_heads":[{"branch":"main"}]
        }"#,
    )
    .fetch_one(&module_pool)
    .await?;
    assert!(complete_baseline_retained);
    let replayed_event = RepoWatchEvent::branch_workflow(
        RepoWatchEventId::from_uuid(Uuid::from_u128(16)),
        repository.clone(),
        default_branch.clone(),
        WorkflowName::try_new(String::from("ci"))?,
        signalbox_session_ownership::CheckConclusion::Success,
    );
    let replayed_occurrence = event_candidate(&replayed_event, identity);
    let replayed_earlier_event = RepoWatchEvent::branch_workflow(
        RepoWatchEventId::from_uuid(Uuid::from_u128(18)),
        repository.clone(),
        default_branch.clone(),
        WorkflowName::try_new(String::from("build"))?,
        signalbox_session_ownership::CheckConclusion::Success,
    );
    let replayed_earlier_occurrence = event_candidate(&replayed_earlier_event, earlier_identity);
    assert_eq!(
        store
            .commit_frontier_candidate(
                &projection,
                0,
                &frontier_entries(&frontier),
                &[replayed_earlier_occurrence, replayed_occurrence],
                EventProducer::Webhook,
                observed_at + Duration::from_secs(1),
            )
            .await?,
        FrontierEventAdmission::Committed {
            generation: 1,
            events: Box::new([EventAdmission::Replayed, EventAdmission::Replayed]),
        }
    );
    let projection_row_versions_before: (String, String) = sqlx::query_as(
        "SELECT repository.xmin::text, pull_request.xmin::text
           FROM repository_state AS repository
           JOIN pr_state AS pull_request USING (repository)
          WHERE repository.repository = $1
            AND pull_request.pull_request_number = $2",
    )
    .bind(repository.as_str())
    .bind(Decimal::from(7_u64))
    .fetch_one(&module_pool)
    .await?;
    assert_eq!(
        store
            .commit_frontier_candidate(
                &projection,
                1,
                &frontier_entries(&frontier),
                &[],
                EventProducer::Poll,
                observed_at + Duration::from_secs(1),
            )
            .await?,
        FrontierEventAdmission::Unchanged
    );
    let projection_row_versions_after: (String, String) = sqlx::query_as(
        "SELECT repository.xmin::text, pull_request.xmin::text
           FROM repository_state AS repository
           JOIN pr_state AS pull_request USING (repository)
          WHERE repository.repository = $1
            AND pull_request.pull_request_number = $2",
    )
    .bind(repository.as_str())
    .bind(Decimal::from(7_u64))
    .fetch_one(&module_pool)
    .await?;
    assert_eq!(
        projection_row_versions_after,
        projection_row_versions_before
    );
    let updated_title = PullRequestTitle::try_new(String::from("Updated projection"))?;
    projection.pull_requests[0].title = &updated_title;
    projection.pull_requests[0].observed_at = observed_at + Duration::from_secs(2);
    assert_eq!(
        store
            .commit_frontier_candidate(
                &projection,
                1,
                &frontier_entries(&frontier),
                &[],
                EventProducer::Poll,
                observed_at + Duration::from_secs(2),
            )
            .await?,
        FrontierEventAdmission::Committed {
            generation: 2,
            events: Box::new([]),
        }
    );
    let stored_title: String = sqlx::query_scalar(
        "SELECT title FROM pr_state
          WHERE repository = $1 AND pull_request_number = $2",
    )
    .bind(repository.as_str())
    .bind(Decimal::from(projection.pull_requests[0].number.get()))
    .fetch_one(&module_pool)
    .await?;
    assert_eq!(stored_title, updated_title.as_str());
    let updated_default_head =
        CommitSha::try_new(String::from("8888888888888888888888888888888888888888"))?;
    projection.repository.default_head = &updated_default_head;
    projection.repository.observed_at = observed_at + Duration::from_secs(3);
    assert_eq!(
        store
            .commit_frontier_candidate(
                &projection,
                2,
                &frontier_entries(&frontier),
                &[],
                EventProducer::Poll,
                observed_at + Duration::from_secs(3),
            )
            .await?,
        FrontierEventAdmission::Committed {
            generation: 3,
            events: Box::new([]),
        }
    );
    let stored_default_head: String =
        sqlx::query_scalar("SELECT default_head_sha FROM repository_state WHERE repository = $1")
            .bind(repository.as_str())
            .fetch_one(&module_pool)
            .await?;
    assert_eq!(stored_default_head, updated_default_head.as_str());
    let eventful_projection_event = RepoWatchEvent::branch_workflow(
        RepoWatchEventId::from_uuid(Uuid::from_u128(17)),
        repository.clone(),
        default_branch.clone(),
        WorkflowName::try_new(String::from("lint"))?,
        signalbox_session_ownership::CheckConclusion::Success,
    );
    let eventful_projection_occurrence = event_candidate(
        &eventful_projection_event,
        RepoWatchEventContentIdentityV1::from_bytes([18; 32]),
    );
    let eventful_projection_title = PullRequestTitle::try_new(String::from("Eventful projection"))?;
    let eventful_projection_default_head =
        CommitSha::try_new(String::from("9999999999999999999999999999999999999999"))?;
    projection.pull_requests[0].title = &eventful_projection_title;
    projection.repository.default_head = &eventful_projection_default_head;
    assert_eq!(
        store
            .commit_frontier_candidate(
                &projection,
                3,
                &frontier_entries(&frontier),
                std::slice::from_ref(&eventful_projection_occurrence),
                EventProducer::Webhook,
                observed_at + Duration::from_secs(4),
            )
            .await?,
        FrontierEventAdmission::Committed {
            generation: 4,
            events: Box::new([EventAdmission::Inserted]),
        }
    );
    let retained_projection: (String, String) = sqlx::query_as(
        "SELECT repository.default_head_sha, pull_request.title
           FROM repository_state AS repository
           JOIN pr_state AS pull_request USING (repository)
          WHERE repository.repository = $1 AND pull_request.pull_request_number = $2",
    )
    .bind(repository.as_str())
    .bind(Decimal::from(projection.pull_requests[0].number.get()))
    .fetch_one(&module_pool)
    .await?;
    assert_eq!(
        retained_projection,
        (
            eventful_projection_default_head.as_str().into(),
            eventful_projection_title.as_str().into()
        )
    );
    let eventful_source: (String, Decimal) = sqlx::query_as(
        "SELECT producer, repository_event_ordinal FROM gh_event WHERE event_id = $1",
    )
    .bind(eventful_projection_event.id().into_uuid())
    .fetch_one(&module_pool)
    .await?;
    assert_eq!(
        eventful_source,
        (String::from("webhook"), Decimal::from(3_u64))
    );
    projection.pull_requests[0].title = &updated_title;
    projection.repository.default_head = &updated_default_head;
    let unchanged_generation: Decimal = sqlx::query_scalar(
        "SELECT frontier_generation FROM repository_state WHERE repository = $1",
    )
    .bind(repository.as_str())
    .fetch_one(&module_pool)
    .await?;
    assert_eq!(unchanged_generation, Decimal::from(4_u64));
    let eventful_projection_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM gh_event WHERE event_id = $1")
            .bind(eventful_projection_event.id().into_uuid())
            .fetch_one(&module_pool)
            .await?;
    assert_eq!(eventful_projection_count, 1);
    let complete_repository = RepositorySlug::try_new(String::from("complete/repository"))?;
    let complete_comparison_baseline = RepoWatchObservation::new(
        Vec::new(),
        RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput::default())?,
    );
    let complete_projection = RepositoryProjection {
        repository: RepositoryState {
            repository: &complete_repository,
            default_branch: &default_branch,
            default_head: &default_head,
            observed_at,
        },
        pull_requests: Vec::new(),
        comparison_baseline: &complete_comparison_baseline,
        merged_baselines: &[],
    };
    let complete_frontier = RepoWatchEventIdentityFrontierV1::try_from_entries(vec![
        RepoWatchEventIdentityFrontierEntryV1::new([20; 32], NonZeroU64::MIN),
        RepoWatchEventIdentityFrontierEntryV1::new([21; 32], NonZeroU64::MIN),
    ])?;
    assert_eq!(
        store
            .commit_frontier_candidate(
                &complete_projection,
                0,
                &frontier_entries(&complete_frontier),
                &[],
                EventProducer::Poll,
                observed_at,
            )
            .await?,
        FrontierEventAdmission::Committed {
            generation: 1,
            events: Box::new([]),
        }
    );
    let incomplete_frontier = RepoWatchEventIdentityFrontierV1::try_from_entries(vec![
        RepoWatchEventIdentityFrontierEntryV1::new([20; 32], NonZeroU64::MIN),
    ])?;
    let incomplete_event = RepoWatchEvent::branch_workflow(
        RepoWatchEventId::from_uuid(Uuid::from_u128(98)),
        complete_repository.clone(),
        default_branch.clone(),
        WorkflowName::try_new(String::from("complete-frontier"))?,
        signalbox_session_ownership::CheckConclusion::Success,
    );
    let incomplete_occurrence = event_candidate(
        &incomplete_event,
        RepoWatchEventContentIdentityV1::from_bytes([22; 32]),
    );
    assert_eq!(
        store
            .commit_frontier_candidate(
                &complete_projection,
                1,
                &frontier_entries(&incomplete_frontier),
                std::slice::from_ref(&incomplete_occurrence),
                EventProducer::Poll,
                observed_at,
            )
            .await?,
        FrontierEventAdmission::Stale
    );
    let (complete_generation, retained_streams): (Decimal, i64) = sqlx::query_as(
        "SELECT repository.frontier_generation, count(frontier.stream_identity)
           FROM repository_state AS repository
           LEFT JOIN frontier USING (repository)
          WHERE repository.repository = $1
          GROUP BY repository.frontier_generation",
    )
    .bind(complete_repository.as_str())
    .fetch_one(&module_pool)
    .await?;
    assert_eq!(complete_generation, Decimal::from(1_u64));
    assert_eq!(retained_streams, 2);
    let incomplete_event_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM gh_event WHERE event_id = $1")
            .bind(incomplete_event.id().into_uuid())
            .fetch_one(&module_pool)
            .await?;
    assert_eq!(incomplete_event_count, 0);
    let incompatible_frontier = RepoWatchEventIdentityFrontierV1::try_from_entries(vec![
        RepoWatchEventIdentityFrontierEntryV1::for_pull_request(
            [19; 32],
            NonZeroU64::MIN,
            PullRequestNumber::new(NonZeroU64::new(7).expect("seven is positive")),
        ),
    ])?;
    assert_eq!(
        store
            .commit_frontier_candidate(
                &projection,
                0,
                &frontier_entries(&incompatible_frontier),
                &[],
                EventProducer::Poll,
                observed_at,
            )
            .await?,
        FrontierEventAdmission::Stale
    );
    let mut ids = FixedDispatchIds {
        value: 16,
        calls: 0,
    };
    let mut factory = FixtureSessionFactory {
        next_command: 17,
        model: 18,
    };
    let plan_batches =
        plan_repository_event(std::slice::from_ref(&rule), &event, &mut ids, &mut factory)?;
    assert_eq!(plan_batches.len(), 1);
    let plans = &plan_batches[0];
    assert_eq!(plans.len(), 2);
    assert_eq!(ids.calls, 1);
    assert_eq!(plans[0].dispatch(), plans[1].dispatch());
    assert_eq!(plans[0].action_ordinal(), 1);
    assert_eq!(plans[1].action_ordinal(), 2);
    let collision_rule = RepoWatchRule::try_new(
        RepoWatchRuleId::try_new(String::from("branch-ci-collision"))?,
        RepoWatchRuleVersion::V1,
        RepoWatchMatcherV1::new(RepoWatchMatcherV1Input {
            event_kinds: vec![RepoWatchEventKindNameV1::BranchWorkflowRunCompleted],
            repository: Some(repository.clone()),
            labels: RepoWatchLabelMatcher::default(),
            ..RepoWatchMatcherV1Input::default()
        }),
        vec![RepoWatchRuleActionV1::DispatchSession {
            template: SessionTemplateName::try_new(String::from("repo-watch-second"))?,
        }],
        RepoWatchSingletonScope::Repository,
        Duration::ZERO,
    )?;
    let mut grouped_ids = FixedDispatchIds {
        value: 40,
        calls: 0,
    };
    let mut grouped_factory = FixtureSessionFactory {
        next_command: 42,
        model: 18,
    };
    let grouped = plan_repository_event(
        &[rule.clone(), collision_rule.clone()],
        &event,
        &mut grouped_ids,
        &mut grouped_factory,
    )?;
    assert_eq!(grouped.len(), 2);
    assert_eq!(grouped[0].len(), 2);
    assert_eq!(grouped[1].len(), 1);
    assert_ne!(grouped[0][0].dispatch(), grouped[1][0].dispatch());
    let signalbox_session_ownership::SessionCommandPayload::CreateSession(created) =
        plans[0].command().clone().into_payload()
    else {
        panic!("matching dispatch action must produce create_session");
    };
    assert_eq!(created.start_gate(), StartGate::Held);
    let retained_dispatch = plans[0].dispatch();
    let retained_command_ids = plans
        .iter()
        .map(|planned| planned.command().command_id())
        .collect::<Vec<_>>();
    assert!(matches!(
        store
            .record_commands(plans, observed_at, &mut command_codec)
            .await?,
        DispatchAdmission::Inserted
    ));
    assert_eq!(
        store
            .reconcile_rules(
                &[RepositoryRuleSet::new(
                    &repository,
                    &[
                        rule.clone(),
                        second_rule.clone(),
                        late_rule.clone(),
                        collision_rule.clone(),
                    ],
                )],
                observed_at,
            )
            .await?,
        RuleReconciliationAdmission::Applied {
            rules: Box::new([
                RuleAdmission::Replayed,
                RuleAdmission::Replayed,
                RuleAdmission::Replayed,
                RuleAdmission::Inserted,
            ]),
            deactivated: 0,
        }
    );
    let mut colliding_ids = FixedDispatchIds {
        value: 16,
        calls: 0,
    };
    let mut colliding_factory = FixtureSessionFactory {
        next_command: 80,
        model: 18,
    };
    let colliding_batches = plan_repository_event(
        std::slice::from_ref(&collision_rule),
        &event,
        &mut colliding_ids,
        &mut colliding_factory,
    )?;
    assert!(matches!(
        store
            .record_commands(&colliding_batches[0], observed_at, &mut command_codec)
            .await?,
        DispatchAdmission::ConflictingReuse
    ));
    let mut occupied_ids = FixedDispatchIds {
        value: 30,
        calls: 0,
    };
    let mut occupied_factory = FixtureSessionFactory {
        next_command: 90,
        model: 18,
    };
    let occupied_batches = plan_repository_event(
        std::slice::from_ref(&rule),
        &earlier_event,
        &mut occupied_ids,
        &mut occupied_factory,
    )?;
    assert!(matches!(
        store
            .record_commands(&occupied_batches[0], observed_at, &mut command_codec)
            .await?,
        DispatchAdmission::Inserted
    ));
    let mut replay_ids = FixedDispatchIds {
        value: 30,
        calls: 0,
    };
    let mut replay_factory = FixtureSessionFactory {
        next_command: 31,
        model: 99,
    };
    let replay_batches = plan_repository_event(
        std::slice::from_ref(&rule),
        &event,
        &mut replay_ids,
        &mut replay_factory,
    )?;
    assert_eq!(replay_batches.len(), 1);
    let replay_plans = &replay_batches[0];
    let mut decode_only_codec = DecodeOnlyCommandCodec;
    let DispatchAdmission::Replayed {
        commands: recovered,
    } = store
        .record_commands(replay_plans, observed_at, &mut decode_only_codec)
        .await?
    else {
        panic!("equal replay must return its retained command batch");
    };
    assert_eq!(recovered.len(), retained_command_ids.len());
    assert!(
        recovered
            .iter()
            .all(|planned| planned.dispatch() == retained_dispatch)
    );
    assert_eq!(
        recovered
            .iter()
            .map(|planned| planned.command().command_id())
            .collect::<Vec<_>>(),
        retained_command_ids
    );
    for recovered in &recovered {
        let SessionCommandPayload::CreateSession(command) =
            recovered.command().clone().into_payload()
        else {
            panic!("retained dispatch command must remain create_session");
        };
        assert_eq!(
            command.initial_configuration_defaults().model(),
            ModelSelectionRequest::Direct(DirectModelSelection::from_uuid(Uuid::from_u128(18)))
        );
    }
    let retained_commands: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM dispatch_ledger
          WHERE repository = $1 AND rule_id = $2 AND event_id = $3
            AND trigger_sequence IS NULL",
    )
    .bind(repository.as_str())
    .bind(rule.id().as_str())
    .bind(event.id().into_uuid())
    .fetch_one(&module_pool)
    .await?;
    assert_eq!(retained_commands, 2);
    assert_eq!(
        store
            .reconcile_rules(
                &[RepositoryRuleSet::new(
                    &repository,
                    &[
                        second_rule.clone(),
                        late_rule.clone(),
                        collision_rule.clone(),
                    ],
                )],
                observed_at,
            )
            .await?,
        RuleReconciliationAdmission::Applied {
            rules: Box::new([
                RuleAdmission::Replayed,
                RuleAdmission::Replayed,
                RuleAdmission::Replayed,
            ]),
            deactivated: 1,
        }
    );
    let DispatchAdmission::Replayed {
        commands: recovered,
    } = store
        .record_commands(replay_plans, observed_at, &mut command_codec)
        .await?
    else {
        panic!("retained commands remain recoverable after deactivation");
    };
    assert_eq!(
        recovered
            .iter()
            .map(|planned| planned.command().command_id())
            .collect::<Vec<_>>(),
        retained_command_ids
    );
    let recovered_without_rule = store.recover_pending_commands(&mut command_codec).await?;
    let recovered_without_removed_rule = recovered_without_rule
        .iter()
        .filter(|planned| planned.rule_id() == rule.id() && planned.event_id() == event.id())
        .collect::<Vec<_>>();
    assert_eq!(
        recovered_without_removed_rule
            .iter()
            .map(|planned| planned.command().command_id())
            .collect::<Vec<_>>(),
        retained_command_ids
    );
    assert!(recovered_without_removed_rule.iter().all(|planned| {
        let SessionCommandPayload::CreateSession(command) =
            planned.command().clone().into_payload()
        else {
            return false;
        };
        command.initial_configuration_defaults().model()
            == ModelSelectionRequest::Direct(DirectModelSelection::from_uuid(Uuid::from_u128(18)))
    }));
    let created_session = Uuid::from_u128(82);
    let lifecycle_command_id = Uuid::from_u128(83);
    let trigger_sequence = NonZeroU64::new(42).expect("forty-two is positive");
    let reaction_session = SessionId::from_uuid(created_session);
    let mismatched_reaction = plan_lifecycle_reaction_for_test(
        NonZeroU64::new(40).expect("forty is positive"),
        SessionId::from_uuid(Uuid::from_u128(81)),
        retained_dispatch,
        &rule,
        &event,
        NonZeroU64::MIN,
        SessionLifecycleCommand::new(
            DurableCommandId::from_uuid(Uuid::from_u128(88)),
            reaction_session,
            SessionLifecycleOperation::ReleaseStart,
        ),
    );
    assert!(matches!(
        mismatched_reaction,
        Err(LifecycleReactionError::MismatchedSession)
    ));
    let unowned_reaction = [plan_lifecycle_reaction_for_test(
        NonZeroU64::new(41).expect("forty-one is positive"),
        reaction_session,
        retained_dispatch,
        &rule,
        &event,
        NonZeroU64::new(3).expect("three is positive"),
        SessionLifecycleCommand::new(
            DurableCommandId::from_uuid(Uuid::from_u128(84)),
            reaction_session,
            SessionLifecycleOperation::ReleaseStart,
        ),
    )?];
    assert!(matches!(
        store
            .record_commands(&unowned_reaction, observed_at, &mut command_codec)
            .await?,
        DispatchAdmission::ConflictingReuse
    ));
    let ordered_reaction_one = plan_lifecycle_reaction_for_test(
        trigger_sequence,
        reaction_session,
        retained_dispatch,
        &rule,
        &event,
        NonZeroU64::MIN,
        SessionLifecycleCommand::new(
            DurableCommandId::from_uuid(Uuid::from_u128(85)),
            reaction_session,
            SessionLifecycleOperation::ReleaseStart,
        ),
    )?;
    let ordered_reaction_two = plan_lifecycle_reaction_for_test(
        trigger_sequence,
        reaction_session,
        retained_dispatch,
        &rule,
        &event,
        NonZeroU64::new(2).expect("two is positive"),
        SessionLifecycleCommand::new(
            DurableCommandId::from_uuid(Uuid::from_u128(86)),
            reaction_session,
            SessionLifecycleOperation::ReleaseStart,
        ),
    )?;
    assert!(matches!(
        store
            .record_commands(
                &[ordered_reaction_two.clone(), ordered_reaction_one.clone()],
                observed_at,
                &mut command_codec,
            )
            .await,
        Err(StoreError::InvalidDispatchBatch)
    ));
    assert!(matches!(
        store
            .record_commands(
                &[ordered_reaction_one.clone(), ordered_reaction_one],
                observed_at,
                &mut command_codec,
            )
            .await,
        Err(StoreError::InvalidDispatchBatch)
    ));
    let reaction_commands = [plan_lifecycle_reaction_for_test(
        trigger_sequence,
        reaction_session,
        retained_dispatch,
        &rule,
        &event,
        NonZeroU64::new(2).expect("two is positive"),
        SessionLifecycleCommand::new(
            DurableCommandId::from_uuid(lifecycle_command_id),
            reaction_session,
            SessionLifecycleOperation::Stop {
                sticky: StopStickiness::Sticky,
                descendant_scope: DescendantTerminationScope::ParentAlone,
            },
        ),
    )?];
    assert!(matches!(
        store
            .record_commands(&reaction_commands, observed_at, &mut command_codec)
            .await?,
        DispatchAdmission::Inserted
    ));
    let retained_reaction_ordinals: Vec<Decimal> = sqlx::query_scalar(
        "SELECT action_ordinal FROM dispatch_ledger
          WHERE dispatch_ref = $1 AND trigger_sequence = 42
          ORDER BY action_ordinal",
    )
    .bind(retained_dispatch.into_uuid())
    .fetch_all(&module_pool)
    .await?;
    assert_eq!(retained_reaction_ordinals, [Decimal::from(2_u64)]);
    let retained_reaction_kind: String = sqlx::query_scalar(
        "SELECT command_kind FROM dispatch_ledger
          WHERE command_id = $1",
    )
    .bind(lifecycle_command_id)
    .fetch_one(&module_pool)
    .await?;
    assert_eq!(retained_reaction_kind, "lifecycle");
    let recovered_reaction = store
        .recover_pending_commands(&mut command_codec)
        .await?
        .into_iter()
        .find(|planned| planned.command().command_id().into_uuid() == lifecycle_command_id)
        .expect("the reaction owed by action two remains queued for submission");
    assert_eq!(recovered_reaction.action_ordinal(), 2);
    let conflicting_create = CreateSessionOutcome::ConflictingReuse {
        command_id: retained_command_ids[0],
    };
    assert!(
        store
            .apply_create_session_outcome(&conflicting_create, observed_at + Duration::from_secs(1))
            .await?
    );
    assert!(
        !store
            .apply_create_session_outcome(&conflicting_create, observed_at + Duration::from_secs(1))
            .await?
    );
    let created_event = LifecycleEvent::session_created_for_test(
        43,
        observed_at + Duration::from_secs(1),
        reaction_session,
        SessionCreated {
            cause: SessionCreationCause::ModuleDispatched {
                dispatch: ModuleDispatch::RepositoryWatch {
                    dispatch: retained_dispatch,
                },
            },
            ownership: SessionOwnership::Owned,
        },
    );
    assert!(store.apply_lifecycle_event(&created_event).await?);
    assert!(!store.apply_lifecycle_event(&created_event).await?);
    let linked_session: Uuid =
        sqlx::query_scalar("SELECT created_session_id FROM dispatch_ledger WHERE command_id = $1")
            .bind(retained_command_ids[1].into_uuid())
            .fetch_one(&module_pool)
            .await?;
    assert_eq!(linked_session, created_session);
    let rejected_create: (String, Option<String>) =
        sqlx::query_as("SELECT status, rejection_kind FROM dispatch_ledger WHERE command_id = $1")
            .bind(retained_command_ids[0].into_uuid())
            .fetch_one(&module_pool)
            .await?;
    assert_eq!(
        rejected_create,
        (
            String::from("rejected"),
            Some(String::from("conflicting_reuse"))
        )
    );
    let restarted_store = RepoWatchStore::new(module_pool.clone());
    let retained_origin = restarted_store
        .reaction_origin_for_session(reaction_session)
        .await?
        .expect("a created module session retains its reaction origin");
    assert_eq!(retained_origin.dispatch(), retained_dispatch);
    assert_eq!(
        retained_origin.action_ordinal(),
        NonZeroU64::new(2).expect("two is positive")
    );
    assert_eq!(retained_origin.repository(), &repository);
    assert_eq!(retained_origin.rule_id(), rule.id());
    assert_eq!(retained_origin.rule_revision(), rule.version());
    assert_eq!(retained_origin.event_id(), event.id());
    assert_eq!(retained_origin.event_kind(), event.kind().name());
    let mismatched_retained_reaction = plan_retained_lifecycle_reaction_for_test(
        NonZeroU64::new(44).expect("forty-four is positive"),
        SessionId::from_uuid(Uuid::from_u128(81)),
        &retained_origin,
        SessionLifecycleCommand::new(
            DurableCommandId::from_uuid(Uuid::from_u128(89)),
            reaction_session,
            SessionLifecycleOperation::ReleaseStart,
        ),
    );
    assert!(matches!(
        mismatched_retained_reaction,
        Err(LifecycleReactionError::MismatchedSession)
    ));
    let restarted_reaction = [plan_retained_lifecycle_reaction_for_test(
        NonZeroU64::new(44).expect("forty-four is positive"),
        reaction_session,
        &retained_origin,
        SessionLifecycleCommand::new(
            DurableCommandId::from_uuid(Uuid::from_u128(87)),
            reaction_session,
            SessionLifecycleOperation::ReleaseStart,
        ),
    )?];
    assert!(matches!(
        restarted_store
            .record_commands(&restarted_reaction, observed_at, &mut command_codec)
            .await?,
        DispatchAdmission::Inserted
    ));
    let still_pending_creates: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM dispatch_ledger
          WHERE dispatch_ref = $1 AND command_kind = 'create_session' AND status = 'pending'",
    )
    .bind(retained_dispatch.into_uuid())
    .fetch_one(&module_pool)
    .await?;
    assert_eq!(still_pending_creates, 0);
    let invalid_lifecycle_link = sqlx::query(
        "UPDATE dispatch_ledger
            SET status = 'applied', settled_at = $2, created_session_id = $3
          WHERE command_id = $1",
    )
    .bind(lifecycle_command_id)
    .bind(observed_at + Duration::from_secs(1))
    .bind(created_session)
    .execute(&module_pool)
    .await;
    assert!(matches!(
        invalid_lifecycle_link,
        Err(sqlx::Error::Database(error)) if error.code().as_deref() == Some("23514")
    ));
    let payload: Vec<u8> =
        sqlx::query_scalar("SELECT normalized_payload FROM gh_event WHERE event_id = $1")
            .bind(event.id().into_uuid())
            .fetch_one(&module_pool)
            .await?;
    let payload = String::from_utf8(payload)?;
    assert!(payload.contains("branch_workflow_run_completed"));
    assert!(payload.contains("\"workflow\":\"ci\""));

    let next_frontier = RepoWatchEventIdentityFrontierV1::try_from_entries(vec![
        RepoWatchEventIdentityFrontierEntryV1::for_pull_request(
            stream,
            NonZeroU64::new(3).expect("three is positive"),
            PullRequestNumber::new(NonZeroU64::new(7).expect("seven is positive")),
        ),
    ])?;
    let preceding_event = RepoWatchEvent::branch_workflow(
        RepoWatchEventId::from_uuid(Uuid::from_u128(15)),
        repository.clone(),
        default_branch.clone(),
        WorkflowName::try_new(String::from("build"))?,
        signalbox_session_ownership::CheckConclusion::Success,
    );
    let preceding_occurrence = event_candidate(
        &preceding_event,
        RepoWatchEventContentIdentityV1::from_bytes([17; 32]),
    );
    let conflicting_occurrence = event_candidate(
        &event,
        RepoWatchEventContentIdentityV1::from_bytes([16; 32]),
    );
    assert_eq!(
        store
            .commit_frontier_candidate(
                &projection,
                4,
                &frontier_entries(&next_frontier),
                &[preceding_occurrence, conflicting_occurrence],
                EventProducer::Poll,
                observed_at,
            )
            .await?,
        FrontierEventAdmission::ConflictingReuse
    );
    let preceding_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM gh_event WHERE event_id = $1")
            .bind(preceding_event.id().into_uuid())
            .fetch_one(&module_pool)
            .await?;
    assert_eq!(preceding_count, 0);
    let frontier_sequence: Decimal = sqlx::query_scalar(
        "SELECT sequence FROM frontier
          WHERE repository = $1 AND stream_identity = $2",
    )
    .bind(repository.as_str())
    .bind(stream.as_slice())
    .fetch_one(&module_pool)
    .await?;
    assert_eq!(frontier_sequence, Decimal::from(2_u64));
    let stale_frontier = RepoWatchEventIdentityFrontierV1::try_from_entries(vec![
        RepoWatchEventIdentityFrontierEntryV1::for_pull_request(
            stream,
            NonZeroU64::MIN,
            PullRequestNumber::new(NonZeroU64::new(7).expect("seven is positive")),
        ),
    ])?;
    assert_eq!(
        store
            .commit_frontier_candidate(
                &projection,
                4,
                &frontier_entries(&stale_frontier),
                &[],
                EventProducer::Poll,
                observed_at,
            )
            .await?,
        FrontierEventAdmission::Stale
    );
    let second_reviewer = RepoWatchAuthorLogin::try_new(String::from("hubot"))?;
    let changed_comparison_baseline = RepoWatchObservation::new(
        vec![author.clone(), second_reviewer],
        comparison_baseline.state().clone(),
    );
    projection.comparison_baseline = &changed_comparison_baseline;
    assert_eq!(
        store
            .commit_frontier_candidate(
                &projection,
                4,
                &frontier_entries(&frontier),
                &[],
                EventProducer::Poll,
                observed_at,
            )
            .await?,
        FrontierEventAdmission::Committed {
            generation: 5,
            events: Box::new([]),
        }
    );
    let committed_digest: Vec<u8> = sqlx::query_scalar(
        "SELECT last_frontier_commit_digest
           FROM repository_state WHERE repository = $1",
    )
    .bind(repository.as_str())
    .fetch_one(&module_pool)
    .await?;
    assert_eq!(
        store.release_frontier(&repository, 4, &stream).await?,
        FrontierReleaseAdmission::Stale
    );
    let sequence_after_stale_release: Decimal = sqlx::query_scalar(
        "SELECT sequence FROM frontier
          WHERE repository = $1 AND stream_identity = $2",
    )
    .bind(repository.as_str())
    .bind(stream.as_slice())
    .fetch_one(&module_pool)
    .await?;
    assert_eq!(sequence_after_stale_release, Decimal::from(2_u64));
    assert_eq!(
        store.release_frontier(&repository, 5, &stream).await?,
        FrontierReleaseAdmission::Released { generation: 6 }
    );
    let (released_generation, released_digest): (Decimal, Vec<u8>) = sqlx::query_as(
        "SELECT frontier_generation, last_frontier_commit_digest
           FROM repository_state WHERE repository = $1",
    )
    .bind(repository.as_str())
    .fetch_one(&module_pool)
    .await?;
    assert_eq!(released_generation, Decimal::from(6_u64));
    assert_ne!(released_digest, committed_digest);
    assert_eq!(
        store
            .commit_frontier_candidate(
                &projection,
                1,
                &frontier_entries(&frontier),
                &[],
                EventProducer::Poll,
                observed_at,
            )
            .await?,
        FrontierEventAdmission::Stale
    );
    assert_eq!(
        store.release_frontier(&repository, 5, &stream).await?,
        FrontierReleaseAdmission::Replayed { generation: 6 }
    );
    assert_eq!(
        store.release_frontier(&repository, 6, &[99; 32]).await?,
        FrontierReleaseAdmission::Absent
    );

    let mut replay = delivery();
    replay.received_at += Duration::from_secs(1);
    replay.expires_at += Duration::from_secs(1);
    assert_eq!(
        store.admit_webhook(replay).await?,
        WebhookAdmission::PendingReplay
    );

    let mut conflict = delivery();
    conflict.body = br#"{"action":"closed"}"#;
    assert_eq!(
        store.admit_webhook(conflict).await?,
        WebhookAdmission::ConflictingReuse
    );
    assert!(
        store
            .settle_webhook(
                9,
                Uuid::from_u128(10),
                WebhookDisposition::Applied,
                observed_at + Duration::from_secs(1),
            )
            .await?
    );
    assert_eq!(
        store.admit_webhook(delivery()).await?,
        WebhookAdmission::Replayed
    );
    assert!(store.advance_core_event(0, 4).await?);
    assert!(store.advance_core_event(4, 9).await?);
    assert!(!store.advance_core_event(4, 10).await?);
    assert!(
        !store
            .settle_webhook(
                9,
                Uuid::from_u128(10),
                WebhookDisposition::Ignored,
                observed_at + Duration::from_secs(2),
            )
            .await?
    );

    let first_unseen_repository = RepositorySlug::try_new(String::from("unseen/first-repository"))?;
    let second_unseen_repository =
        RepositorySlug::try_new(String::from("unseen/second-repository"))?;
    let first_unseen_configuration = [RepositoryRuleSet::new(
        &first_unseen_repository,
        std::slice::from_ref(&rule),
    )];
    let second_unseen_configuration = [RepositoryRuleSet::new(
        &second_unseen_repository,
        std::slice::from_ref(&rule),
    )];
    let (first_unseen, second_unseen) = tokio::join!(
        store.reconcile_rules(&first_unseen_configuration, observed_at),
        store.reconcile_rules(&second_unseen_configuration, observed_at)
    );
    assert!(matches!(
        (first_unseen?, second_unseen?),
        (
            RuleReconciliationAdmission::Applied { .. },
            RuleReconciliationAdmission::Applied { .. }
        )
    ));
    let active_unseen_repositories: Vec<String> = sqlx::query_scalar(
        "SELECT repository FROM rule
          WHERE active_revision IS NOT NULL AND repository LIKE 'unseen/%'
          ORDER BY repository",
    )
    .fetch_all(&module_pool)
    .await?;
    assert_eq!(active_unseen_repositories.len(), 1);
    assert!(
        active_unseen_repositories[0] == first_unseen_repository.as_str()
            || active_unseen_repositories[0] == second_unseen_repository.as_str()
    );
    let rebuild_stream = [20; 32];
    let retained_frontier = RepoWatchEventIdentityFrontierV1::try_from_entries(vec![
        RepoWatchEventIdentityFrontierEntryV1::for_pull_request(
            rebuild_stream,
            NonZeroU64::new(2).expect("two is positive"),
            PullRequestNumber::new(NonZeroU64::new(7).expect("seven is positive")),
        ),
    ])?;
    let recurring_event = RepoWatchEvent::branch_workflow(
        RepoWatchEventId::from_uuid(Uuid::from_u128(20)),
        repository.clone(),
        default_branch.clone(),
        WorkflowName::try_new(String::from("recurring"))?,
        signalbox_session_ownership::CheckConclusion::Success,
    );
    let recurring_occurrence = event_candidate(
        &recurring_event,
        RepoWatchEventContentIdentityV1::from_bytes([20; 32]),
    );
    assert_eq!(
        store
            .commit_frontier_candidate(
                &projection,
                6,
                &frontier_entries(&retained_frontier),
                &[recurring_occurrence],
                EventProducer::Poll,
                observed_at + Duration::from_secs(5),
            )
            .await?,
        FrontierEventAdmission::Committed {
            generation: 7,
            events: Box::new([EventAdmission::Inserted]),
        }
    );
    let accepted_event_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM gh_event WHERE repository = $1")
            .bind(repository.as_str())
            .fetch_one(&module_pool)
            .await?;
    assert!(accepted_event_count > 0);
    let accepted_event_generation: i64 = sqlx::query_scalar(
        "SELECT max(frontier_generation)::bigint FROM gh_event WHERE repository = $1",
    )
    .bind(repository.as_str())
    .fetch_one(&module_pool)
    .await?;
    sqlx::query("DELETE FROM repository_state WHERE repository = $1")
        .bind(repository.as_str())
        .execute(&module_pool)
        .await?;
    let retained_after_projection_delete: (i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM repository_state WHERE repository = $1),
            (SELECT count(*) FROM gh_event WHERE repository = $1),
            (SELECT count(*) FROM frontier WHERE repository = $1)",
    )
    .bind(repository.as_str())
    .fetch_one(&module_pool)
    .await?;
    assert_eq!(
        retained_after_projection_delete,
        (0, accepted_event_count, 1)
    );

    let restored_frontier = RepoWatchEventIdentityFrontierV1::try_from_entries(vec![
        RepoWatchEventIdentityFrontierEntryV1::for_pull_request(
            rebuild_stream,
            NonZeroU64::new(3).expect("three is positive"),
            PullRequestNumber::new(NonZeroU64::new(7).expect("seven is positive")),
        ),
    ])?;

    let restored_event = RepoWatchEvent::branch_workflow(
        RepoWatchEventId::from_uuid(Uuid::from_u128(19)),
        repository.clone(),
        default_branch.clone(),
        WorkflowName::try_new(String::from("recurring"))?,
        signalbox_session_ownership::CheckConclusion::Success,
    );
    let restored_occurrence = event_candidate(
        &restored_event,
        RepoWatchEventContentIdentityV1::from_bytes([19; 32]),
    );
    let restored_generation = u64::try_from(accepted_event_generation)? + 1;
    assert_eq!(
        store
            .commit_frontier_candidate(
                &projection,
                0,
                &frontier_entries(&restored_frontier),
                &[restored_occurrence],
                EventProducer::Poll,
                observed_at + Duration::from_secs(6),
            )
            .await?,
        FrontierEventAdmission::Committed {
            generation: restored_generation,
            events: Box::new([EventAdmission::Inserted]),
        }
    );
    let restored_coordinates: (Decimal, Decimal) = sqlx::query_as(
        "SELECT frontier_generation, event_ordinal
           FROM gh_event WHERE event_id = $1",
    )
    .bind(restored_event.id().into_uuid())
    .fetch_one(&module_pool)
    .await?;
    assert_eq!(
        restored_coordinates,
        (Decimal::from(restored_generation), Decimal::from(1_u64))
    );
    assert_eq!(
        store
            .commit_frontier_candidate(
                &projection,
                0,
                &frontier_entries(&restored_frontier),
                &[restored_occurrence],
                EventProducer::Poll,
                observed_at + Duration::from_secs(6),
            )
            .await?,
        FrontierEventAdmission::Committed {
            generation: restored_generation,
            events: Box::new([EventAdmission::Replayed]),
        }
    );
    let intervening_title = PullRequestTitle::try_new(String::from("Intervening projection"))?;
    projection.pull_requests[0].title = &intervening_title;
    assert_eq!(
        store
            .commit_frontier_candidate(
                &projection,
                restored_generation,
                &frontier_entries(&restored_frontier),
                &[],
                EventProducer::Poll,
                observed_at + Duration::from_secs(7),
            )
            .await?,
        FrontierEventAdmission::Committed {
            generation: restored_generation + 1,
            events: Box::new([]),
        }
    );
    projection.pull_requests[0].title = &eventful_projection_title;
    assert_eq!(
        store
            .commit_frontier_candidate(
                &projection,
                restored_generation + 1,
                &frontier_entries(&restored_frontier),
                &[restored_occurrence],
                EventProducer::Poll,
                observed_at + Duration::from_secs(8),
            )
            .await?,
        FrontierEventAdmission::Committed {
            generation: restored_generation + 2,
            events: Box::new([EventAdmission::Replayed]),
        }
    );
    assert_eq!(
        store
            .commit_frontier_candidate(
                &projection,
                0,
                &frontier_entries(&restored_frontier),
                &[restored_occurrence],
                EventProducer::Poll,
                observed_at + Duration::from_secs(8),
            )
            .await?,
        FrontierEventAdmission::Stale
    );
    let retained_sequence: Decimal = sqlx::query_scalar(
        "SELECT sequence FROM frontier
          WHERE repository = $1 AND stream_identity = $2",
    )
    .bind(repository.as_str())
    .bind(rebuild_stream.as_slice())
    .fetch_one(&module_pool)
    .await?;
    assert_eq!(retained_sequence, Decimal::from(3_u64));

    // A separate repository exercises the runtime's empty-store and restart path.
    let runtime_repository = RepositorySlug::try_new(String::from("runtime-restart/project"))?;
    let empty_baseline = store.ingest_baseline(&runtime_repository).await?;
    assert_eq!(empty_baseline.generation, 0);
    assert!(empty_baseline.observation.is_none());
    let observed = signalbox_module_repo_watch_v2::ingest::RepositoryObservation {
        merged_at: std::collections::BTreeMap::new(),
        repository: runtime_repository.clone(),
        default_branch: default_branch.clone(),
        default_head: default_head.clone(),
        observation: comparison_baseline.clone(),
        observed_at,
    };
    assert!(matches!(
        store
            .ingest_observation(
                &empty_baseline,
                &observed,
                EventProducer::Webhook,
                MERGED_RETENTION
            )
            .await?,
        FrontierEventAdmission::Committed { generation: 1, .. }
    ));
    let restarted = RepoWatchStore::new(module_pool.clone());
    let restart_baseline = restarted.ingest_baseline(&runtime_repository).await?;
    assert_eq!(
        restart_baseline.observation.as_ref(),
        Some(&comparison_baseline)
    );
    assert_eq!(
        restarted
            .ingest_observation(
                &restart_baseline,
                &observed,
                EventProducer::Poll,
                MERGED_RETENTION
            )
            .await?,
        FrontierEventAdmission::Unchanged
    );
    let retry = restarted
        .ingest_observation(
            &empty_baseline,
            &observed,
            EventProducer::Poll,
            MERGED_RETENTION,
        )
        .await?;
    let FrontierEventAdmission::Committed { generation, events } = retry else {
        panic!("an identical retry must recover its committed frontier");
    };
    assert_eq!(generation, 1);
    assert!(!events.is_empty());
    assert!(
        events
            .iter()
            .all(|event| *event == EventAdmission::Replayed)
    );
    let lineage: (i64, bool) = sqlx::query_as(
        "SELECT count(*), bool_and(producer = 'webhook'
                AND frontier_generation = 1 AND repository_event_ordinal = event_ordinal)
           FROM gh_event WHERE repository = $1",
    )
    .bind(runtime_repository.as_str())
    .fetch_one(&module_pool)
    .await?;
    assert!(lineage.0 > 0);
    assert!(lineage.1);
    let measurements = store.clone().ingestion_measurements(&runtime_repository);
    assert_eq!(measurements.last_successful_observation, Some(observed_at));
    assert_eq!(measurements.events_recorded, u64::try_from(lineage.0)?);
    assert_eq!(measurements.last_poll, None);
    assert_eq!(
        restarted
            .ingestion_measurements(&runtime_repository)
            .events_recorded,
        0,
        "replays and unchanged observations record no new events"
    );

    for (repository_name, lifecycle, expected_kind, expected_compact_count) in [
        (
            "terminal-closed/project",
            RepoWatchPullRequestLifecycle::Closed,
            "pull_request_closed",
            0,
        ),
        (
            "terminal-merged/project",
            RepoWatchPullRequestLifecycle::Merged,
            "pull_request_merged",
            1,
        ),
    ] {
        let repository = RepositorySlug::try_new(repository_name.to_owned())?;
        let mut terminal_observation = observed.clone();
        terminal_observation.repository = repository.clone();
        store
            .ingest_observation(
                &store.ingest_baseline(&repository).await?,
                &terminal_observation,
                EventProducer::Poll,
                MERGED_RETENTION,
            )
            .await?;
        let source = &terminal_observation.observation.state().pull_requests()[0];
        let terminal = ComparisonPullRequestState::try_new(RepoWatchPullRequestStateInput {
            context: source.context().clone(),
            lifecycle,
            mergeable_state: source.mergeable_state(),
            completed_check_suites: source.completed_check_suites().to_vec(),
            completed_check_runs: source.completed_check_runs().to_vec(),
            reviews: source.reviews().to_vec(),
            threads: source.threads().to_vec(),
            reactions: source.reactions().to_vec(),
        })?;
        terminal_observation.merged_at.insert(
            terminal.context().number(),
            observed_at.replace_nanosecond(0)?,
        );
        terminal_observation.observation = RepoWatchObservation::new(
            terminal_observation.observation.signal_reviewers().to_vec(),
            RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput {
                pull_requests: vec![terminal],
                workflow_runs: terminal_observation
                    .observation
                    .state()
                    .workflow_runs()
                    .to_vec(),
                branch_heads: terminal_observation
                    .observation
                    .state()
                    .branch_heads()
                    .to_vec(),
            })?,
        );
        store
            .ingest_observation(
                &store.ingest_baseline(&repository).await?,
                &terminal_observation,
                EventProducer::Poll,
                MERGED_RETENTION,
            )
            .await?;
        let retained = RepoWatchStore::new(module_pool.clone())
            .ingest_baseline(&repository)
            .await?;
        assert!(
            retained
                .observation
                .as_ref()
                .expect("retained observation")
                .state()
                .pull_requests()
                .is_empty(),
            "{repository_name}: terminal subjects leave the ordinary baseline"
        );
        assert_eq!(
            retained.merged_baselines.len(),
            expected_compact_count,
            "{repository_name}"
        );
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM gh_event WHERE repository = $1 AND event_kind = $2",
        )
        .bind(repository.as_str())
        .bind(expected_kind)
        .fetch_one(&module_pool)
        .await?;
        assert_eq!(
            count, 1,
            "{repository_name}: retirement preserves its terminal fact"
        );
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM pr_state WHERE repository = $1")
            .bind(repository.as_str())
            .fetch_one(&module_pool)
            .await?;
        assert_eq!(
            count, 0,
            "{repository_name}: retirement deletes the ordinary projection"
        );
    }

    let compact_repository = RepositorySlug::try_new(String::from("compacted-restart/project"))?;
    let source = &comparison_baseline.state().pull_requests()[0];
    let merged = ComparisonPullRequestState::try_new(RepoWatchPullRequestStateInput {
        context: source.context().clone(),
        lifecycle: RepoWatchPullRequestLifecycle::Merged,
        mergeable_state: source.mergeable_state(),
        completed_check_suites: source.completed_check_suites().to_vec(),
        completed_check_runs: source.completed_check_runs().to_vec(),
        reviews: source.reviews().to_vec(),
        threads: source.threads().to_vec(),
        reactions: source.reactions().to_vec(),
    })?;
    let compacted =
        signalbox_session_ownership::RepoWatchMergedPullRequestBaselineV1::from_merged_state(
            &merged,
            comparison_baseline.signal_reviewers(),
        )?
        .expect("merged baseline");
    let merge_time = observed_at.replace_nanosecond(0)?;
    let compacted = signalbox_module_repo_watch_v2::ingest::MergedPullRequestBaseline {
        state: compacted,
        merged_at: merge_time,
    };
    let compact_observation = RepoWatchObservation::new(
        comparison_baseline.signal_reviewers().to_vec(),
        RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput {
            pull_requests: Vec::new(),
            workflow_runs: Vec::new(),
            branch_heads: vec![RepoWatchBranchHead::new(
                default_branch.clone(),
                default_head.clone(),
            )],
        })?,
    );
    let compact_projection = RepositoryProjection {
        repository: RepositoryState {
            repository: &compact_repository,
            default_branch: &default_branch,
            default_head: &default_head,
            observed_at,
        },
        pull_requests: Vec::new(),
        comparison_baseline: &compact_observation,
        merged_baselines: std::slice::from_ref(&compacted),
    };
    store
        .commit_frontier_candidate(
            &compact_projection,
            0,
            &[],
            &[],
            EventProducer::Poll,
            observed_at,
        )
        .await?;
    let reopened = RepoWatchStore::new(module_pool.clone());
    let restored = reopened.ingest_baseline(&compact_repository).await?;
    assert_eq!(restored.merged_baselines, vec![compacted]);
    let mut completed_check_runs = merged.completed_check_runs().to_vec();
    // A distinct completed run is the only change after compaction and restart.
    completed_check_runs.push(RepoWatchCheckRunObservation::new(
        GitHubObjectId::new(NonZeroU64::new(900001).expect("new fixture check identity")),
        RepoWatchCheckCompletionGeneration::try_new(String::from("post-merge-completion"))?,
        CheckRunName::try_new(String::from("post-merge"))?,
        CheckConclusion::Success,
    ));
    let changed = ComparisonPullRequestState::try_new(RepoWatchPullRequestStateInput {
        context: merged.context().clone(),
        lifecycle: RepoWatchPullRequestLifecycle::Merged,
        mergeable_state: merged.mergeable_state(),
        completed_check_suites: merged.completed_check_suites().to_vec(),
        completed_check_runs,
        reviews: merged.reviews().to_vec(),
        threads: merged.threads().to_vec(),
        reactions: merged.reactions().to_vec(),
    })?;
    let next = signalbox_module_repo_watch_v2::ingest::RepositoryObservation {
        merged_at: std::collections::BTreeMap::from([(merged.context().number(), merge_time)]),
        repository: compact_repository.clone(),
        default_branch: default_branch.clone(),
        default_head: default_head.clone(),
        observed_at,
        observation: RepoWatchObservation::new(
            comparison_baseline.signal_reviewers().to_vec(),
            RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput {
                pull_requests: vec![changed],
                workflow_runs: Vec::new(),
                branch_heads: compact_observation.state().branch_heads().to_vec(),
            })?,
        ),
    };
    reopened
        .ingest_observation(&restored, &next, EventProducer::Poll, MERGED_RETENTION)
        .await?;
    let kinds: Vec<String> = sqlx::query_scalar(
        "SELECT event_kind FROM gh_event WHERE repository = $1 ORDER BY repository_event_ordinal",
    )
    .bind(compact_repository.as_str())
    .fetch_all(&module_pool)
    .await?;
    assert_eq!(kinds, vec![String::from("check_run_completed")]);
    let retained = reopened.ingest_baseline(&compact_repository).await?;
    let compact_payload: String = sqlx::query_scalar(
        "SELECT comparison_baseline::text FROM repository_state WHERE repository = $1",
    )
    .bind(compact_repository.as_str())
    .fetch_one(&module_pool)
    .await?;
    assert!(
        retained
            .observation
            .as_ref()
            .expect("committed ordinary observation")
            .state()
            .pull_requests()
            .is_empty(),
        "refetched compact pull requests remain outside the ordinary baseline"
    );
    let ordinary_rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM pr_state WHERE repository = $1")
            .bind(compact_repository.as_str())
            .fetch_one(&module_pool)
            .await?;
    assert_eq!(
        ordinary_rows, 0,
        "refetch does not recreate ordinary PR rows"
    );
    assert_eq!(retained.merged_baselines.len(), 1);
    assert_eq!(
        retained.merged_baselines[0]
            .state
            .completed_check_runs()
            .len(),
        merged.completed_check_runs().len() + 1
    );
    assert_eq!(
        reopened
            .ingest_observation(&retained, &next, EventProducer::Poll, MERGED_RETENTION)
            .await?,
        FrontierEventAdmission::Unchanged
    );

    let mut expiry_observation = next.clone();
    expiry_observation.observation = compact_observation.clone();
    expiry_observation.merged_at.clear();
    expiry_observation.observed_at = merge_time + MERGED_RETENTION - Duration::from_secs(1);
    reopened
        .ingest_observation(
            &retained,
            &expiry_observation,
            EventProducer::Poll,
            MERGED_RETENTION,
        )
        .await?;
    let before_expiry = RepoWatchStore::new(module_pool.clone())
        .ingest_baseline(&compact_repository)
        .await?;
    assert_eq!(
        before_expiry.merged_baselines.len(),
        1,
        "the compact baseline survives until its merge-time deadline"
    );
    expiry_observation.observed_at = merge_time + MERGED_RETENTION;
    reopened
        .ingest_observation(
            &before_expiry,
            &expiry_observation,
            EventProducer::Poll,
            MERGED_RETENTION,
        )
        .await?;
    let at_expiry = RepoWatchStore::new(module_pool.clone())
        .ingest_baseline(&compact_repository)
        .await?;
    assert!(
        at_expiry.merged_baselines.is_empty(),
        "the compact baseline expires at merge time plus the configured bound"
    );
    let event_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM gh_event WHERE repository = $1")
            .bind(compact_repository.as_str())
            .fetch_one(&module_pool)
            .await?;
    assert_eq!(
        event_count, 1,
        "compaction expiry does not delete immutable events"
    );

    let mut missing_merge_time: serde_json::Value = serde_json::from_str(&compact_payload)?;
    missing_merge_time["merged_pull_requests"][0]
        .as_object_mut()
        .expect("compact baseline")
        .remove("merged_at");
    sqlx::query(
        "UPDATE repository_state SET comparison_baseline = $2::jsonb WHERE repository = $1",
    )
    .bind(compact_repository.as_str())
    .bind(missing_merge_time.to_string())
    .execute(&module_pool)
    .await?;
    let invalidated = reopened.ingest_baseline(&compact_repository).await?;
    assert_eq!(invalidated.observation, at_expiry.observation);
    assert!(invalidated.merged_baselines.is_empty());
    assert_eq!(invalidated.generation, at_expiry.generation);
    assert_eq!(invalidated.frontier, at_expiry.frontier);
    assert!(matches!(
        reopened
            .ingest_observation(&invalidated, &next, EventProducer::Poll, MERGED_RETENTION)
            .await?,
        FrontierEventAdmission::Committed { .. }
    ));
    let rebuilt = RepoWatchStore::new(module_pool.clone())
        .ingest_baseline(&compact_repository)
        .await?;
    assert!(rebuilt.observation.is_some());
    assert_eq!(rebuilt.merged_baselines[0].merged_at, merge_time);

    module_pool.close().await;
    core_pool.close().await;
    drop(container);
    Ok(())
}

impl signalbox_module_repo_watch_v2::dispatch::LifecycleCommandFactory for FixtureSessionFactory {
    fn lifecycle(
        &mut self,
        session: SessionId,
        operation: SessionLifecycleOperation,
    ) -> SessionLifecycleCommand {
        let id = DurableCommandId::from_uuid(Uuid::from_u128(self.next_command));
        self.next_command += 1;
        SessionLifecycleCommand::new(id, session, operation)
    }
}

fn dispatch_observation(
    repository: &RepositorySlug,
    run: u64,
    now: OffsetDateTime,
) -> signalbox_module_repo_watch_v2::ingest::RepositoryObservation {
    // Distinct run identities produce successive facts on one unchanged branch.
    let branch = BranchName::try_new(String::from("main")).expect("branch");
    let head =
        CommitSha::try_new(String::from("1111111111111111111111111111111111111111")).expect("head");
    signalbox_module_repo_watch_v2::ingest::RepositoryObservation {
        merged_at: std::collections::BTreeMap::new(),
        repository: repository.clone(),
        default_branch: branch.clone(),
        default_head: head.clone(),
        observed_at: now,
        observation: RepoWatchObservation::new(
            Vec::new(),
            RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput {
                pull_requests: Vec::new(),
                branch_heads: vec![RepoWatchBranchHead::new(branch.clone(), head)],
                workflow_runs: vec![RepoWatchWorkflowRunObservation::new(
                    GitHubObjectId::new(NonZeroU64::new(run).expect("positive run")),
                    GitHubObjectId::new(NonZeroU64::new(1).expect("workflow")),
                    RepoWatchWorkflowRunAttempt::new(NonZeroU64::new(1).expect("attempt")),
                    branch,
                    WorkflowName::try_new(String::from("CI")).expect("workflow"),
                    CheckConclusion::Success,
                )],
            })
            .expect("complete observation"),
        ),
    }
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn dispatch_resumes_after_activation_and_enforces_singleton_release_and_cooldown()
-> Result<(), Box<dyn Error>> {
    use signalbox_session_ownership::{
        GoalChange, GoalEventKind, LifecycleActor, LifecycleEventKind, SessionStateKind,
        SessionTerminal, SessionTerminalOutcome,
    };
    let (container, core_pool, url) = postgres().await?;
    migrate(&core_pool).await?;
    let source = signalbox_session_ownership::LifecycleEventSource::new(core_pool.clone());
    sqlx::query("ALTER ROLE mod_repo_watch PASSWORD 'signalbox-test-only'")
        .execute(&core_pool)
        .await?;
    let pool = module_pool(&url).await?;
    sqlx::query("CREATE FUNCTION reject_redundant_cursor_update() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN IF NEW.event_ordinal = OLD.event_ordinal THEN RAISE EXCEPTION 'redundant evaluation cursor update'; END IF; RETURN NEW; END $$")
        .execute(&pool).await?;
    sqlx::query(
        "CREATE TRIGGER reject_redundant_cursor_update BEFORE UPDATE ON rule_evaluation_cursor
        FOR EACH ROW EXECUTE FUNCTION reject_redundant_cursor_update()",
    )
    .execute(&pool)
    .await?;
    let store = RepoWatchStore::new(pool.clone());
    let repository = RepositorySlug::try_new(String::from("dispatch/project"))?;
    let now = OffsetDateTime::now_utc();
    let initial = dispatch_observation(&repository, 1, now);
    store
        .ingest_observation(
            &store.ingest_baseline(&repository).await?,
            &initial,
            EventProducer::Poll,
            MERGED_RETENTION,
        )
        .await?;
    let rule = RepoWatchRule::try_new(
        RepoWatchRuleId::try_new(String::from("ci"))?,
        RepoWatchRuleVersion::V1,
        RepoWatchMatcherV1::new(RepoWatchMatcherV1Input {
            event_kinds: vec![RepoWatchEventKindNameV1::BranchWorkflowRunCompleted],
            repository: Some(repository.clone()),
            ..RepoWatchMatcherV1Input::default()
        }),
        vec![RepoWatchRuleActionV1::DispatchSession {
            template: SessionTemplateName::try_new(String::from("watch"))?,
        }],
        RepoWatchSingletonScope::Repository,
        Duration::from_secs(5),
    )?;
    store
        .reconcile_rules(
            &[RepositoryRuleSet::new(
                &repository,
                std::slice::from_ref(&rule),
            )],
            now,
        )
        .await?;
    assert!(
        store.next_rule_event(&repository, &rule).await?.is_none(),
        "activation excludes already-admitted facts"
    );
    let mut ids = FixedDispatchIds {
        value: 10001,
        calls: 0,
    };
    let mut factory = FixtureSessionFactory {
        next_command: 20001,
        model: 30001,
    };
    let mut codec = FixtureCommandCodec;
    for run in [2, 3] {
        let observation = dispatch_observation(&repository, run, now);
        store
            .ingest_observation(
                &store.ingest_baseline(&repository).await?,
                &observation,
                EventProducer::Poll,
                MERGED_RETENTION,
            )
            .await?;
        assert!(
            store
                .evaluate_next(&repository, &rule, &mut ids, &mut factory, &mut codec, now)
                .await
                .expect("evaluate")
        );
    }
    let commands = store.recover_pending_commands(&mut codec).await?;
    assert_eq!(
        commands.len(),
        1,
        "the second matching fact is suppressed while the singleton is held"
    );
    let first = &commands[0];
    let session = SessionId::from_uuid(Uuid::from_u128(40001));
    let creation = LifecycleEvent::session_created_for_test(
        1,
        now,
        session,
        SessionCreated {
            cause: SessionCreationCause::ModuleDispatched {
                dispatch: ModuleDispatch::RepositoryWatch {
                    dispatch: first.dispatch(),
                },
            },
            ownership: SessionOwnership::Owned,
        },
    );
    store
        .react_to_lifecycle(&creation, &mut factory, &mut codec, &source)
        .await?;
    let goal = LifecycleEvent::for_test(
        2,
        now,
        Some(session),
        LifecycleEventKind::GoalChanged(GoalChange {
            event_ordinal: 1,
            generation: 1,
            kind: GoalEventKind::Commissioned,
        }),
    );
    store
        .react_to_lifecycle(&goal, &mut factory, &mut codec, &source)
        .await?;
    store
        .react_to_lifecycle(&goal, &mut factory, &mut codec, &source)
        .await?;
    let reactions = store.recover_pending_commands(&mut codec).await?;
    assert_eq!(reactions.len(), 1, "a replay retains one reaction identity");
    assert!(
        matches!(reactions[0].command().clone().into_payload(),SessionCommandPayload::Lifecycle(command) if *command.operation() == SessionLifecycleOperation::ReleaseStart)
    );
    let release_id = reactions[0].command().command_id();
    let mut sink = SettlingThenFailingSink {
        store: store.clone(),
        fail: true,
        calls: Vec::new(),
        now,
    };
    assert!(
        store
            .submit_pending(&mut codec, &mut sink, &source)
            .await
            .is_err()
    );
    assert_eq!(
        store.recover_pending_commands(&mut codec).await?.len(),
        1,
        "a committed command still retries an unfinished submission follow-up"
    );
    sink.fail = false;
    store
        .submit_pending(&mut codec, &mut sink, &source)
        .await
        .expect("retry follow-up");
    assert_eq!(
        sink.calls,
        vec![release_id, release_id],
        "retry uses the exact command identity"
    );
    assert!(store.recover_pending_commands(&mut codec).await?.is_empty());
    let terminal = LifecycleEvent::for_test(
        3,
        now,
        Some(session),
        LifecycleEventKind::SessionTerminal(SessionTerminal {
            prior: SessionStateKind::Created,
            outcome: SessionTerminalOutcome::AchievedVerified,
            standing: None,
            actor: LifecycleActor::Operator,
        }),
    );
    store
        .react_to_lifecycle(&terminal, &mut factory, &mut codec, &source)
        .await?;
    let restarted = RepoWatchStore::new(pool.clone());
    assert!(
        restarted
            .next_rule_event(&repository, &rule)
            .await?
            .is_none(),
        "suppressed facts stay consumed across restart"
    );
    for (run, elapsed, expected_creations) in [(4, 4, 1), (5, 5, 2)] {
        let time = now + Duration::from_secs(elapsed);
        let observation = dispatch_observation(&repository, run, time);
        restarted
            .ingest_observation(
                &restarted.ingest_baseline(&repository).await?,
                &observation,
                EventProducer::Poll,
                MERGED_RETENTION,
            )
            .await?;
        assert!(
            restarted
                .evaluate_next(&repository, &rule, &mut ids, &mut factory, &mut codec, time)
                .await
                .expect("evaluate")
        );
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM dispatch_ledger WHERE trigger_sequence IS NULL",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            count, expected_creations,
            "cooldown begins when the previous singleton releases"
        );
    }
    store
        .reconcile_rules(&[], now + Duration::from_secs(6))
        .await?;
    let stop = LifecycleEvent::for_test(
        4,
        now + Duration::from_secs(6),
        Some(session),
        LifecycleEventKind::GoalChanged(GoalChange {
            event_ordinal: 2,
            generation: 1,
            kind: GoalEventKind::UserStopped,
        }),
    );
    restarted
        .react_to_lifecycle(&stop, &mut factory, &mut codec, &source)
        .await?;
    assert!(restarted.recover_pending_commands(&mut codec).await?.iter().any(|p| matches!(p.command().clone().into_payload(),SessionCommandPayload::Lifecycle(command) if matches!(command.operation(),SessionLifecycleOperation::Stop { sticky:StopStickiness::Sticky, .. }))),"removed rules still retain their lifecycle reaction origin");
    pool.close().await;
    core_pool.close().await;
    drop(container);
    Ok(())
}

struct SettlingThenFailingSink {
    store: RepoWatchStore,
    fail: bool,
    calls: Vec<DurableCommandId>,
    now: OffsetDateTime,
}
impl signalbox_module_repo_watch_v2::dispatch::SessionCommandSink for SettlingThenFailingSink {
    type Error = ();
    async fn submit(
        &mut self,
        command: SessionCommand,
    ) -> Result<signalbox_module_repo_watch_v2::dispatch::CommandSubmission, ()> {
        use signalbox_session_ownership::{CommandSettlement, LifecycleEventKind};
        let SessionCommandPayload::Lifecycle(command) = command.into_payload() else {
            panic!("fixture submits only its start release");
        };
        self.calls.push(command.command_id());
        let event = LifecycleEvent::for_test(
            99,
            self.now,
            Some(command.session()),
            LifecycleEventKind::CommandSettled {
                command: command.command_id(),
                result: CommandSettlement::Applied,
            },
        );
        self.store
            .apply_lifecycle_event(&event)
            .await
            .expect("core settlement");
        if self.fail {
            Err(())
        } else {
            Ok(signalbox_module_repo_watch_v2::dispatch::CommandSubmission::Accepted)
        }
    }
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn repository_watch_creation_records_its_module_issuer() -> Result<(), Box<dyn Error>> {
    use signalbox_application::{InProcessEligibilityWorkSource, InProcessToolDispatchGate};
    use signalbox_module_repo_watch_v2::dispatch::{CommandSubmission, SessionCommandSink};
    use signalbox_persistence::scheduler::PostgresEligibilitySweep;
    use signalboxd::{HubModelConfiguration, repo_watch_dispatch::RepositoryWatchCommandSink};
    use std::sync::Arc;

    let (container, pool, _) = postgres().await?;
    migrate(&pool).await?;
    let models = HubModelConfiguration::parse(
        &include_str!("../../../config/signalboxd.example.toml").replace(
            "/usr/local/bin/signalbox-exec-supervisor",
            std::env::current_exe()?.to_string_lossy().as_ref(),
        ),
    )?;
    let (eligibility_nudge, _work_source) =
        InProcessEligibilityWorkSource::new(PostgresEligibilitySweep::new(pool.clone()));
    let mut sink = RepositoryWatchCommandSink {
        goal_resumption: signalboxd::PostgresGoalPassDisposition::new(
            pool.clone(),
            models.clone(),
            eligibility_nudge.clone(),
            signalboxd::GoalModeNumericBounds::new(None, None, None, None, None),
        ),
        checkout_runner: None,
        pool: pool.clone(),
        models: Arc::new(models),
        eligibility_nudge,
        tool_dispatch_gate: InProcessToolDispatchGate::default(),
    };
    let id = DurableCommandId::from_uuid(Uuid::now_v7());
    let command = SessionCommand::create_session(
        CreateSession::new(
            id,
            SessionCreationProvenance::module_dispatched(ModuleDispatch::RepositoryWatch {
                dispatch: RepoWatchDispatchId::from_uuid(Uuid::now_v7()),
            }),
            SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(
                DirectModelSelection::from_uuid(Uuid::now_v7()),
            )),
        )
        .with_lifecycle(
            StartGate::Held,
            SessionOwnership::Owned,
            Some(FinishCondition::ExternalGate),
        ),
    )
    .expect("held seam command");
    assert!(matches!(
        sink.submit(command.clone()).await.expect("create session"),
        CommandSubmission::Creation(CreateSessionOutcome::Applied(_))
    ));
    assert!(matches!(
        sink.submit(command).await.expect("replay creation"),
        CommandSubmission::Creation(CreateSessionOutcome::Applied(_))
    ));
    let issuer: (String, Option<String>) = sqlx::query_as(
        "SELECT issuer_kind, issuer_module FROM durable_command WHERE command_id = $1",
    )
    .bind(id.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        issuer,
        (String::from("module"), Some(String::from("repo_watch")))
    );
    let held: bool = sqlx::query_scalar("SELECT start_gate_held FROM session_lifecycle")
        .fetch_one(&pool)
        .await?;
    assert!(held);
    let inputs: i64 = sqlx::query_scalar("SELECT count(*) FROM accepted_input")
        .fetch_one(&pool)
        .await?;
    assert_eq!(inputs, 0);
    pool.close().await;
    drop(container);
    Ok(())
}

struct RuntimeHookFixture<'a> {
    address: std::net::SocketAddr,
    path: &'a str,
    id: u64,
    secret: &'a std::path::Path,
    enabled: bool,
    rule_version: u64,
    template: &'a str,
    mode: &'a str,
    retention: &'a str,
}

fn runtime_configuration(
    hook: &RuntimeHookFixture<'_>,
) -> Result<signalboxd::HubModelConfiguration, Box<dyn Error>> {
    Ok(signalboxd::HubModelConfiguration::parse(
        &runtime_configuration_source(hook)?,
    )?)
}

fn runtime_configuration_source(hook: &RuntimeHookFixture<'_>) -> Result<String, Box<dyn Error>> {
    let poll_credential = hook.secret.with_extension("poll-token");
    write_private_credential(&poll_credential, b"")?;
    let mut catalog = include_str!("../../../config/signalboxd.example.toml")
        .replace(
            "/usr/local/bin/signalbox-exec-supervisor",
            std::env::current_exe()?.to_string_lossy().as_ref(),
        )
        .replace(
            "repository_watch_webhook_retention = \"604800s\"",
            &format!("repository_watch_webhook_retention = {:?}", hook.retention),
        );
    for profile in ["anthropic-primary", "anthropic-overflow"] {
        let model_credential = hook
            .secret
            .parent()
            .expect("fixture directory")
            .join(profile);
        write_private_credential(&model_credential, b"")?;
        catalog = catalog.replace(
            &format!("/run/secrets/{profile}"),
            model_credential.to_str().expect("fixture credential path"),
        );
    }
    Ok(format!(
        r#"{catalog}
[repository_watch]
version = 1
enabled = {enabled}
signal_reviewers = []
[repository_watch.webhook]
bind_address = "{address}"
path = "{path}"
[[repository_watch.repositories]]
repository = "runtime/project"
poll_interval_seconds = 60
credential_file = "{poll_credential}"
webhook_hook_id = {id}
webhook_secret_file = "{secret}"
webhook_mode = "{mode}"
[[repository_watch.rules]]
id = "ci"
version = {rule_version}
singleton_per = "repo"
cooldown_seconds = 0
[repository_watch.rules.matcher]
event_kinds = ["branch_workflow_run_completed"]
[[repository_watch.rules.actions]]
kind = "dispatch_session"
template = "{template}"
"#,
        enabled = hook.enabled,
        rule_version = hook.rule_version,
        template = hook.template,
        mode = hook.mode,
        address = hook.address,
        path = hook.path,
        id = hook.id,
        secret = hook.secret.display(),
        poll_credential = poll_credential.display(),
    ))
}

/// Synthetic secrets use the same private permissions as admitted credentials.
fn write_private_credential(
    path: &std::path::Path,
    bytes: impl AsRef<[u8]>,
) -> std::io::Result<()> {
    std::fs::write(path, bytes)?;
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600))
}

async fn unused_webhook_address() -> Result<std::net::SocketAddr, std::io::Error> {
    tokio::net::TcpListener::bind("127.0.0.1:0")
        .await?
        .local_addr()
}

const RUNTIME_WEBHOOK_BODY: &str = r#"{"repository":{"full_name":"Runtime/Project"}}"#;

struct RuntimeWebhookDelivery<'a> {
    id: Uuid,
    event: &'a str,
    body: &'a str,
}

async fn webhook_status(
    hook: &RuntimeHookFixture<'_>,
    secret: &[u8],
) -> Result<reqwest::StatusCode, reqwest::Error> {
    webhook_delivery_status(
        hook,
        secret,
        &RuntimeWebhookDelivery {
            id: Uuid::now_v7(),
            event: "push",
            body: RUNTIME_WEBHOOK_BODY,
        },
    )
    .await
}

/// Waits until the HTTP handler's insert is blocked by the fixture's table lock.
async fn wait_for_blocked_webhook_admission(pool: &PgPool) -> Result<(), Box<dyn Error>> {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT FROM pg_locks
                 WHERE database = (SELECT oid FROM pg_database WHERE datname = current_database())
                   AND relation = 'webhook_delivery'::regclass AND NOT granted)",
            )
            .fetch_one(pool)
            .await?;
            if waiting {
                return Ok::<_, sqlx::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    Ok(())
}

/// Waits until rule activation is blocked after the listener pause.
async fn wait_for_blocked_reload_activation(pool: &PgPool) -> Result<(), Box<dyn Error>> {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT FROM pg_locks
                 WHERE database = (SELECT oid FROM pg_database WHERE datname = current_database())
                   AND relation = 'reload_activation'::regclass AND NOT granted)",
            )
            .fetch_one(pool)
            .await?;
            if waiting {
                return Ok::<_, sqlx::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    Ok(())
}

/// Waits until reconciliation cannot yet restore eligibility through its target transaction.
async fn wait_for_blocked_target_reconciliation(pool: &PgPool) -> Result<(), Box<dyn Error>> {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT FROM pg_locks
                 WHERE database = (SELECT oid FROM pg_database WHERE datname = current_database())
                   AND relation = 'convergence_sweep_target'::regclass AND NOT granted)",
            )
            .fetch_one(pool)
            .await?;
            if waiting {
                return Ok::<_, sqlx::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    Ok(())
}

async fn webhook_delivery_status(
    hook: &RuntimeHookFixture<'_>,
    secret: &[u8],
    delivery: &RuntimeWebhookDelivery<'_>,
) -> Result<reqwest::StatusCode, reqwest::Error> {
    let signature = ring::hmac::sign(
        &ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret),
        delivery.body.as_bytes(),
    );
    let _ = rustls::crypto::ring::default_provider().install_default();
    Ok(reqwest::Client::builder()
        .no_proxy()
        .build()?
        .post(format!("http://{}{}", hook.address, hook.path))
        .header("x-github-hook-id", hook.id)
        .header("x-github-delivery", delivery.id.to_string())
        .header("x-github-event", delivery.event)
        .header(
            "x-hub-signature-256",
            format!("sha256={}", hex::encode(signature.as_ref())),
        )
        .body(delivery.body.to_owned())
        .send()
        .await?
        .status())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn composed_repository_watch_dispatches_and_reloads_its_running_listener()
-> Result<(), Box<dyn Error>> {
    use signalbox_application::{InProcessEligibilityWorkSource, InProcessToolDispatchGate};
    use signalbox_persistence::scheduler::PostgresEligibilitySweep;
    use signalboxd::{
        SessionTemplateConfiguration,
        repo_watch_runtime::{
            RepositoryWatchRuntime, RepositoryWatchServices, connect_repository_watch_pool,
        },
    };
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (container, core_pool, _) = unmigrated_postgres().await?;
    migrate(&core_pool).await?;
    let module_pool = connect_repository_watch_pool(&core_pool)
        .await
        .expect("independently authenticated module pool");
    let user: String = sqlx::query_scalar("SELECT current_user")
        .fetch_one(&module_pool)
        .await?;
    assert_eq!(user, "mod_repo_watch");
    assert!(
        sqlx::query("SELECT * FROM public.session_lifecycle")
            .fetch_all(&module_pool)
            .await
            .is_err(),
        "module login cannot read core session tables"
    );
    let files = tempfile::tempdir()?;
    let secret = files.path().join("hook-secret");
    write_private_credential(&secret, b"initial-hook-secret")?;
    let mut hook = RuntimeHookFixture {
        address: unused_webhook_address().await?,
        path: "/initial",
        id: 17,
        secret: &secret,
        enabled: false,
        rule_version: 1,
        template: "watch",
        mode: "primary",
        retention: "604800s",
    };
    let models = runtime_configuration(&hook)?;
    let template_path = files.path().join("templates.toml");
    std::fs::write(
        &template_path,
        r#"version = 1
[[templates]]
name = "watch"
version = 1
alias = "540ce009-c2ec-4a04-b823-c411ea189778"
dangerous_tool_auto_approval = false
system_prompt = "Inspect repository activity."
"#,
    )?;
    let templates = SessionTemplateConfiguration::read(&template_path, || None, &models)?;
    let (eligibility_nudge, _work_source) =
        InProcessEligibilityWorkSource::new(PostgresEligibilitySweep::new(core_pool.clone()));
    let runtime = RepositoryWatchRuntime::new(
        module_pool.clone(),
        models.repository_watch().cloned(),
        RepositoryWatchServices {
            goal_resumption: signalboxd::PostgresGoalPassDisposition::new(
                core_pool.clone(),
                models.clone(),
                eligibility_nudge.clone(),
                signalboxd::GoalModeNumericBounds::new(None, None, None, None, None),
            ),
            checkout_runner: None,
            core_pool: core_pool.clone(),
            models: Arc::new(models),
            templates: Arc::new(templates),
            eligibility_nudge,
            tool_dispatch_gate: InProcessToolDispatchGate::default(),
        },
    )
    .await
    .expect("prepare runtime");
    let (shutdown, stopped) = tokio::sync::watch::channel(false);
    let worker = tokio::spawn(runtime.clone().run(stopped));
    assert!(
        tokio::net::TcpStream::connect(hook.address).await.is_err(),
        "disabled module has no listener"
    );
    hook.enabled = true;
    runtime
        .reload_configuration(runtime_configuration(&hook)?.repository_watch().cloned())
        .await
        .expect("enable during reload");
    assert_eq!(
        webhook_status(&hook, b"wrong-secret").await?,
        reqwest::StatusCode::UNAUTHORIZED
    );
    let unauthenticated_rows: i64 = sqlx::query_scalar("SELECT count(*) FROM webhook_delivery")
        .fetch_one(&module_pool)
        .await?;
    assert_eq!(
        unauthenticated_rows, 0,
        "unauthenticated payloads are not retained"
    );
    let delivery = RuntimeWebhookDelivery {
        id: Uuid::now_v7(),
        event: "pull_request",
        body: r#"{ "action":"opened", "repository":{"full_name":"Runtime/Project"} }"#,
    };
    assert_eq!(
        webhook_delivery_status(&hook, b"initial-hook-secret", &delivery).await?,
        reqwest::StatusCode::ACCEPTED
    );
    #[derive(Debug, PartialEq, sqlx::FromRow)]
    struct RetainedWebhook {
        repository: String,
        event_kind: String,
        action: Option<String>,
        body: Vec<u8>,
        disposition: String,
        received_at: OffsetDateTime,
        expires_at: OffsetDateTime,
        settled_at: Option<OffsetDateTime>,
    }
    let retained_delivery = || {
        sqlx::query_as::<_, RetainedWebhook>(
            "SELECT delivery.repository, delivery.event_kind, delivery.action,
                body.body, disposition.disposition, delivery.received_at,
                delivery.expires_at, disposition.settled_at
         FROM webhook_delivery AS delivery
         JOIN webhook_body AS body USING (hook_id, delivery_id)
         JOIN webhook_disposition AS disposition USING (hook_id, delivery_id)
         WHERE delivery.hook_id = $1 AND delivery.delivery_id = $2",
        )
        .bind(Decimal::from(hook.id))
        .bind(delivery.id)
    };
    let retained = retained_delivery().fetch_one(&module_pool).await?;
    assert_eq!(retained.repository, "runtime/project");
    assert_eq!(retained.event_kind, delivery.event);
    assert_eq!(retained.action.as_deref(), Some("opened"));
    assert_eq!(retained.body, delivery.body.as_bytes());
    assert_eq!(retained.disposition, "applied");
    assert!(retained.settled_at.is_some());
    assert_eq!(
        (retained.expires_at - retained.received_at).whole_seconds(),
        7 * 24 * 60 * 60
    );
    assert_eq!(
        webhook_delivery_status(&hook, b"initial-hook-secret", &delivery).await?,
        reqwest::StatusCode::ACCEPTED,
        "equal delivery identities replay"
    );
    assert_eq!(retained_delivery().fetch_one(&module_pool).await?, retained);
    let conflicting = RuntimeWebhookDelivery {
        body: r#"{"action":"closed","repository":{"full_name":"Runtime/Project"}}"#,
        ..delivery
    };
    assert_eq!(
        webhook_delivery_status(&hook, b"initial-hook-secret", &conflicting).await?,
        reqwest::StatusCode::CONFLICT
    );
    assert_eq!(retained_delivery().fetch_one(&module_pool).await?, retained);

    {
        // The persisted webhook_body CHECK admits exactly this many bytes.
        const PERSISTED_BODY_CEILING: usize = 26_214_400;
        let mut ceiling_body = RUNTIME_WEBHOOK_BODY.to_owned();
        ceiling_body.extend(std::iter::repeat_n(
            ' ',
            PERSISTED_BODY_CEILING - ceiling_body.len(),
        ));
        let ceiling_delivery = RuntimeWebhookDelivery {
            id: Uuid::now_v7(),
            event: "push",
            body: &ceiling_body,
        };
        assert_eq!(
            webhook_delivery_status(&hook, b"initial-hook-secret", &ceiling_delivery).await?,
            reqwest::StatusCode::ACCEPTED,
            "authenticated bodies at the persistence ceiling are admitted"
        );
        let stored_body: Vec<u8> = sqlx::query_scalar(
            "SELECT body FROM webhook_body WHERE hook_id = $1 AND delivery_id = $2",
        )
        .bind(Decimal::from(hook.id))
        .bind(ceiling_delivery.id)
        .fetch_one(&module_pool)
        .await?;
        assert!(
            stored_body == ceiling_body.as_bytes(),
            "the entire ceiling-sized body is retained"
        );

        ceiling_body.push(' ');
        let oversized_delivery = RuntimeWebhookDelivery {
            id: Uuid::now_v7(),
            event: "push",
            body: &ceiling_body,
        };
        assert_eq!(
            webhook_delivery_status(&hook, b"initial-hook-secret", &oversized_delivery).await?,
            reqwest::StatusCode::PAYLOAD_TOO_LARGE,
            "one byte above the persistence ceiling is rejected"
        );
        let oversized_rows: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM webhook_delivery WHERE hook_id = $1 AND delivery_id = $2",
        )
        .bind(Decimal::from(hook.id))
        .bind(oversized_delivery.id)
        .fetch_one(&module_pool)
        .await?;
        assert_eq!(oversized_rows, 0);
    }

    let store = RepoWatchStore::new(module_pool.clone());
    let repository = RepositorySlug::try_new(String::from("runtime/project"))?;
    for run in [1, 2] {
        store
            .ingest_observation(
                &store.ingest_baseline(&repository).await?,
                &dispatch_observation(&repository, run, OffsetDateTime::now_utc()),
                EventProducer::Poll,
                MERGED_RETENTION,
            )
            .await?;
    }
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let count: i64 = sqlx::query_scalar("SELECT count(*) FROM session_lifecycle")
                .fetch_one(&core_pool)
                .await?;
            let applied: Decimal =
                sqlx::query_scalar("SELECT applied_through FROM core_event_cursor")
                    .fetch_one(&module_pool)
                    .await?;
            if count == 1 && applied > Decimal::ZERO {
                return Ok::<_, sqlx::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    let held: bool = sqlx::query_scalar("SELECT start_gate_held FROM session_lifecycle")
        .fetch_one(&core_pool)
        .await?;
    assert!(held);
    let inputs: i64 = sqlx::query_scalar("SELECT count(*) FROM accepted_input")
        .fetch_one(&core_pool)
        .await?;
    assert_eq!(inputs, 0);

    let occupied = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let occupied_hook = RuntimeHookFixture {
        address: occupied.local_addr()?,
        ..hook
    };
    assert!(
        runtime
            .reload_configuration(
                runtime_configuration(&occupied_hook)?
                    .repository_watch()
                    .cloned()
            )
            .await
            .is_err()
    );
    assert_eq!(
        webhook_status(&hook, b"initial-hook-secret").await?,
        reqwest::StatusCode::ACCEPTED,
        "failed replacement bind preserves running settings"
    );

    // Expect/continue proves the old server admitted the request before replacement.
    let mut inflight = tokio::net::TcpStream::connect(hook.address).await?;
    let replacement_secret = files.path().join("replacement-secret");
    write_private_credential(&replacement_secret, b"replacement-hook-secret")?;
    let signature = ring::hmac::sign(
        &ring::hmac::Key::new(ring::hmac::HMAC_SHA256, b"replacement-hook-secret"),
        RUNTIME_WEBHOOK_BODY.as_bytes(),
    );
    inflight.write_all(format!("POST {} HTTP/1.1\r\nHost: {}\r\nExpect: 100-continue\r\nConnection: close\r\nContent-Length: {}\r\nx-github-hook-id: {}\r\nx-github-delivery: {}\r\nx-github-event: push\r\nx-hub-signature-256: sha256={}\r\n\r\n", hook.path, hook.address, RUNTIME_WEBHOOK_BODY.len(), hook.id, Uuid::now_v7(), hex::encode(signature.as_ref())).as_bytes()).await?;
    let mut interim = [0; 25];
    tokio::time::timeout(Duration::from_secs(5), inflight.read_exact(&mut interim)).await??;
    assert_eq!(&interim, b"HTTP/1.1 100 Continue\r\n\r\n");
    hook.address = unused_webhook_address().await?;
    hook.secret = &replacement_secret;
    runtime
        .reload_configuration(runtime_configuration(&hook)?.repository_watch().cloned())
        .await
        .expect("rebind during delivery");
    inflight.write_all(RUNTIME_WEBHOOK_BODY.as_bytes()).await?;
    let mut response = String::new();
    tokio::time::timeout(
        Duration::from_secs(5),
        inflight.read_to_string(&mut response),
    )
    .await??;
    assert!(
        response.starts_with("HTTP/1.1 202"),
        "in-flight delivery uses replacement hook map: {response}"
    );
    assert_eq!(
        webhook_status(&hook, b"replacement-hook-secret").await?,
        reqwest::StatusCode::ACCEPTED
    );
    let old_path = RuntimeHookFixture { ..hook };
    hook.path = "/replacement";
    hook.id = 18;
    runtime
        .reload_configuration(runtime_configuration(&hook)?.repository_watch().cloned())
        .await
        .expect("swap path and hook map on same socket");
    assert_eq!(
        webhook_status(&old_path, b"replacement-hook-secret").await?,
        reqwest::StatusCode::NOT_FOUND
    );
    let old_id = RuntimeHookFixture {
        id: old_path.id,
        ..hook
    };
    assert_eq!(
        webhook_status(&old_id, b"replacement-hook-secret").await?,
        reqwest::StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        webhook_status(&hook, b"replacement-hook-secret").await?,
        reqwest::StatusCode::ACCEPTED
    );
    hook.mode = "shadow";
    hook.retention = "172800s";
    runtime
        .reload_configuration(runtime_configuration(&hook)?.repository_watch().cloned())
        .await
        .expect("switch to shadow intake");
    let shadow = RuntimeWebhookDelivery {
        id: Uuid::now_v7(),
        ..delivery
    };
    assert_eq!(
        webhook_delivery_status(&hook, b"replacement-hook-secret", &shadow).await?,
        reqwest::StatusCode::ACCEPTED
    );
    let shadow_disposition: String = sqlx::query_scalar(
        "SELECT disposition FROM webhook_disposition WHERE hook_id = $1 AND delivery_id = $2",
    )
    .bind(Decimal::from(hook.id))
    .bind(shadow.id)
    .fetch_one(&module_pool)
    .await?;
    assert_eq!(shadow_disposition, "ignored");
    let shadow_retention: i64 = sqlx::query_scalar(
        "SELECT extract(epoch FROM expires_at - received_at)::bigint
         FROM webhook_delivery WHERE hook_id = $1 AND delivery_id = $2",
    )
    .bind(Decimal::from(hook.id))
    .bind(shadow.id)
    .fetch_one(&module_pool)
    .await?;
    assert_eq!(
        shadow_retention,
        2 * 24 * 60 * 60,
        "reload changes the configured retention"
    );
    hook.mode = "primary";
    runtime
        .reload_configuration(runtime_configuration(&hook)?.repository_watch().cloned())
        .await
        .expect("switch settled shadow delivery to primary intake");
    assert_eq!(
        webhook_delivery_status(&hook, b"replacement-hook-secret", &shadow).await?,
        reqwest::StatusCode::ACCEPTED
    );
    let retained_shadow = || WebhookDelivery {
        repository: &repository,
        hook_id: hook.id,
        delivery_id: shadow.id,
        event: shadow.event,
        action: Some("opened"),
        body: shadow.body.as_bytes(),
        received_at: retained.received_at,
        expires_at: retained.expires_at,
    };
    assert_eq!(
        store.admit_webhook(retained_shadow()).await?,
        WebhookAdmission::Replayed
    );
    let replayed_disposition: String = sqlx::query_scalar(
        "SELECT disposition FROM webhook_disposition WHERE hook_id = $1 AND delivery_id = $2",
    )
    .bind(Decimal::from(hook.id))
    .bind(shadow.id)
    .fetch_one(&module_pool)
    .await?;
    assert_eq!(replayed_disposition, "ignored");

    let pending = RuntimeWebhookDelivery {
        id: Uuid::now_v7(),
        ..shadow
    };
    assert_eq!(
        store
            .admit_webhook(WebhookDelivery {
                delivery_id: pending.id,
                ..retained_shadow()
            })
            .await?,
        WebhookAdmission::Inserted
    );
    assert_eq!(
        store
            .admit_webhook(WebhookDelivery {
                delivery_id: pending.id,
                ..retained_shadow()
            })
            .await?,
        WebhookAdmission::PendingReplay
    );
    assert_eq!(
        webhook_delivery_status(&hook, b"replacement-hook-secret", &pending).await?,
        reqwest::StatusCode::ACCEPTED
    );
    let pending_disposition: String = sqlx::query_scalar(
        "SELECT disposition FROM webhook_disposition WHERE hook_id = $1 AND delivery_id = $2",
    )
    .bind(Decimal::from(hook.id))
    .bind(pending.id)
    .fetch_one(&module_pool)
    .await?;
    assert_eq!(
        pending_disposition, "applied",
        "pending replay completes primary intake"
    );
    let rotated_secret = files.path().join("admission-reload-secret");
    const ROTATED_SECRET: &[u8] = b"admission-reload-secret";
    write_private_credential(&rotated_secret, ROTATED_SECRET)?;
    let shadow_rotated_hook = RuntimeHookFixture {
        mode: "shadow",
        secret: &rotated_secret,
        ..hook
    };
    let inflight_delivery = RuntimeWebhookDelivery {
        id: Uuid::now_v7(),
        ..delivery
    };
    let mut admission_lock = module_pool.begin().await?;
    sqlx::query("LOCK TABLE webhook_delivery IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *admission_lock)
        .await?;
    let (inflight_response, reload) = tokio::join!(
        webhook_delivery_status(&hook, b"replacement-hook-secret", &inflight_delivery),
        async {
            wait_for_blocked_webhook_admission(&module_pool).await?;
            tokio::time::timeout(
                Duration::from_secs(10),
                runtime.reload_configuration(
                    runtime_configuration(&shadow_rotated_hook)?
                        .repository_watch()
                        .cloned(),
                ),
            )
            .await?
            .expect("same-address reload completes while admission is blocked");
            admission_lock.commit().await?;
            Ok::<_, Box<dyn Error>>(())
        }
    );
    reload?;
    assert_eq!(
        inflight_response?,
        reqwest::StatusCode::UNAUTHORIZED,
        "admission revalidates a secret rotated away during PostgreSQL I/O"
    );
    let inflight_disposition = || {
        sqlx::query_scalar::<_, String>(
            "SELECT disposition FROM webhook_disposition WHERE hook_id = $1 AND delivery_id = $2",
        )
        .bind(Decimal::from(hook.id))
        .bind(inflight_delivery.id)
    };
    assert_eq!(
        inflight_disposition().fetch_one(&module_pool).await?,
        "pending",
        "stale primary routing cannot settle or wake the admitted delivery"
    );
    assert_eq!(
        webhook_delivery_status(&shadow_rotated_hook, ROTATED_SECRET, &inflight_delivery).await?,
        reqwest::StatusCode::ACCEPTED
    );
    assert_eq!(
        inflight_disposition().fetch_one(&module_pool).await?,
        "ignored",
        "authenticated retry follows the reloaded shadow mode"
    );
    runtime
        .reload_configuration(runtime_configuration(&hook)?.repository_watch().cloned())
        .await
        .expect("restore primary routing");
    module_pool.close().await;
    assert_eq!(
        webhook_status(&hook, b"replacement-hook-secret").await?,
        reqwest::StatusCode::SERVICE_UNAVAILABLE,
        "durable admission failure cannot acknowledge a delivery"
    );
    shutdown.send(true)?;
    worker.await?.expect("orderly module shutdown");
    assert!(module_pool.is_closed());
    assert!(tokio::net::TcpStream::connect(hook.address).await.is_err());
    core_pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn repository_watch_rejects_invalid_reloads_and_keeps_dispatching_running_rules()
-> Result<(), Box<dyn Error>> {
    use signalbox_application::{InProcessEligibilityWorkSource, InProcessToolDispatchGate};
    use signalbox_persistence::scheduler::PostgresEligibilitySweep;
    use signalboxd::{
        SessionTemplateConfiguration,
        repo_watch_runtime::{
            RepositoryWatchRuntime, RepositoryWatchRuntimeError, RepositoryWatchServices,
            connect_repository_watch_pool,
        },
    };
    use std::sync::Arc;

    let (container, core_pool, _) = unmigrated_postgres().await?;
    migrate(&core_pool).await?;
    let module_pool = connect_repository_watch_pool(&core_pool)
        .await
        .expect("module login");
    let files = tempfile::tempdir()?;
    let secret = files.path().join("hook-secret");
    write_private_credential(&secret, b"hook-secret")?;
    let mut hook = RuntimeHookFixture {
        address: unused_webhook_address().await?,
        path: "/running",
        id: 17,
        secret: &secret,
        enabled: true,
        rule_version: 1,
        template: "watch",
        mode: "primary",
        retention: "604800s",
    };
    let models = Arc::new(runtime_configuration(&hook)?);
    let template_path = files.path().join("templates.toml");
    std::fs::write(
        &template_path,
        r#"version = 1
[[templates]]
name = "watch"
version = 1
alias = "540ce009-c2ec-4a04-b823-c411ea189778"
dangerous_tool_auto_approval = false
system_prompt = "Inspect repository activity."
[[templates]]
name = "alternate"
version = 1
alias = "540ce009-c2ec-4a04-b823-c411ea189778"
dangerous_tool_auto_approval = false
system_prompt = "Inspect workflow failures."
"#,
    )?;
    let templates = Arc::new(SessionTemplateConfiguration::read(
        &template_path,
        || None,
        &models,
    )?);
    let (eligibility_nudge, _work_source) =
        InProcessEligibilityWorkSource::new(PostgresEligibilitySweep::new(core_pool.clone()));
    let services = || RepositoryWatchServices {
        goal_resumption: signalboxd::PostgresGoalPassDisposition::new(
            core_pool.clone(),
            (*models).clone(),
            eligibility_nudge.clone(),
            signalboxd::GoalModeNumericBounds::new(None, None, None, None, None),
        ),
        checkout_runner: None,
        core_pool: core_pool.clone(),
        models: models.clone(),
        templates: templates.clone(),
        eligibility_nudge: eligibility_nudge.clone(),
        tool_dispatch_gate: InProcessToolDispatchGate::default(),
    };
    let missing = RuntimeHookFixture {
        template: "missing",
        ..hook
    };
    assert!(matches!(
        RepositoryWatchRuntime::new(
            module_pool.clone(),
            runtime_configuration(&missing)?.repository_watch().cloned(),
            services(),
        )
        .await,
        Err(RepositoryWatchRuntimeError::Rules)
    ));
    let rules: i64 = sqlx::query_scalar("SELECT count(*) FROM rule")
        .fetch_one(&module_pool)
        .await?;
    assert_eq!(rules, 0, "invalid composition admits no rules");
    let runtime = RepositoryWatchRuntime::new(
        module_pool.clone(),
        models.repository_watch().cloned(),
        services(),
    )
    .await
    .expect("valid composition after rejected templates");
    let (shutdown, stopped) = tokio::sync::watch::channel(false);
    let worker = tokio::spawn(runtime.clone().run(stopped));
    let conflicting = RuntimeHookFixture {
        template: "alternate",
        ..hook
    };
    for rejected in [missing, conflicting] {
        let replacement = RuntimeHookFixture {
            path: "/rejected",
            ..rejected
        };
        assert_eq!(
            runtime
                .reload_configuration(
                    runtime_configuration(&replacement)?
                        .repository_watch()
                        .cloned()
                )
                .await,
            Err(RepositoryWatchRuntimeError::Rules),
            "missing templates and conflicting revision reuse both fail reload"
        );
        assert_eq!(
            webhook_status(&hook, b"hook-secret").await?,
            reqwest::StatusCode::ACCEPTED,
            "rejected reload retains the running listener"
        );
    }

    let store = RepoWatchStore::new(module_pool.clone());
    let repository = RepositorySlug::try_new(String::from("runtime/project"))?;
    store
        .ingest_observation(
            &store.ingest_baseline(&repository).await?,
            &dispatch_observation(&repository, 1, OffsetDateTime::now_utc()),
            EventProducer::Poll,
            MERGED_RETENTION,
        )
        .await?;
    for run in [2, 3] {
        if run == 3 {
            let stale = RuntimeHookFixture {
                path: "/stale",
                ..hook
            };
            hook.rule_version = 2;
            hook.template = "alternate";
            runtime
                .reload_configuration(runtime_configuration(&hook)?.repository_watch().cloned())
                .await
                .expect("apply next revision");
            assert_eq!(
                runtime
                    .reload_configuration(
                        runtime_configuration(&stale)?.repository_watch().cloned()
                    )
                    .await,
                Err(RepositoryWatchRuntimeError::Rules),
                "historical revision fails reload"
            );
            assert_eq!(
                webhook_status(&hook, b"hook-secret").await?,
                reqwest::StatusCode::ACCEPTED
            );
        }
        store
            .ingest_observation(
                &store.ingest_baseline(&repository).await?,
                &dispatch_observation(&repository, run, OffsetDateTime::now_utc()),
                EventProducer::Poll,
                MERGED_RETENTION,
            )
            .await?;
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let sessions: Vec<Uuid> = sqlx::query_scalar(
                    "SELECT created_session_id FROM dispatch_ledger
                     WHERE rule_revision = $1 AND created_session_id IS NOT NULL",
                )
                .bind(Decimal::from(hook.rule_version))
                .fetch_all(&module_pool)
                .await?;
                let count: i64 = sqlx::query_scalar(
                    "SELECT count(*) FROM session
                     WHERE template_name = $1 AND session_id = ANY($2)",
                )
                .bind(hook.template)
                .bind(sessions)
                .fetch_one(&core_pool)
                .await?;
                if count == 1 {
                    return Ok::<_, sqlx::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await??;
        let active: Decimal = sqlx::query_scalar("SELECT active_revision FROM rule")
            .fetch_one(&module_pool)
            .await?;
        assert_eq!(active, Decimal::from(hook.rule_version));
    }
    shutdown.send(true)?;
    worker
        .await?
        .expect("orderly shutdown after rejected reloads");
    core_pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn durable_reload_replays_activated_intent_and_disables_live_workers()
-> Result<(), Box<dyn Error>> {
    use signalbox_application::{InProcessEligibilityWorkSource, InProcessToolDispatchGate};
    use signalbox_module_repo_watch_v2::{ReloadIntentInput, repository_rule_set_digest};
    use signalbox_persistence::{
        reload_configuration::{
            ReloadClaim, ReloadConfiguration, ReloadConfigurationRepository, ReloadIntent,
            ReloadLookup, ReloadPhase, ReloadResult,
        },
        scheduler::PostgresEligibilitySweep,
    };
    use signalboxd::{
        SessionTemplateConfiguration,
        configuration_reload::ConfigurationReload,
        repo_watch_runtime::{
            RepositoryWatchRuntime, RepositoryWatchServices, connect_repository_watch_pool,
        },
    };
    use std::sync::Arc;

    let (_container, core_pool, _) = unmigrated_postgres().await?;
    migrate(&core_pool).await?;
    let module_pool = connect_repository_watch_pool(&core_pool)
        .await
        .expect("module pool");
    let files = tempfile::tempdir()?;
    let secret = files.path().join("secret");
    write_private_credential(&secret, "hook-secret")?;
    let mut hook = RuntimeHookFixture {
        address: unused_webhook_address().await?,
        path: "/reload",
        id: 17,
        secret: &secret,
        enabled: false,
        rule_version: 1,
        template: "watch",
        mode: "shadow",
        retention: "604800s",
    };
    let prior_source = runtime_configuration_source(&hook)?;
    let models = runtime_configuration(&hook)?;
    let templates_source = "version = 1\n[[templates]]\nname = \"watch\"\nversion = 1\nalias = \"540ce009-c2ec-4a04-b823-c411ea189778\"\ndangerous_tool_auto_approval = false\nsystem_prompt = \"Inspect repository activity.\"\n";
    let template_path = files.path().join("templates.toml");
    std::fs::write(&template_path, templates_source)?;
    let templates = SessionTemplateConfiguration::read(&template_path, || None, &models)?;
    let model_path = files.path().join("models.toml");
    let (nudge, _work) =
        InProcessEligibilityWorkSource::new(PostgresEligibilitySweep::new(core_pool.clone()));
    let runtime = RepositoryWatchRuntime::unstarted(
        module_pool.clone(),
        RepositoryWatchServices {
            goal_resumption: signalboxd::PostgresGoalPassDisposition::new(
                core_pool.clone(),
                models.clone(),
                nudge.clone(),
                signalboxd::GoalModeNumericBounds::new(None, None, None, None, None),
            ),
            checkout_runner: None,
            core_pool: core_pool.clone(),
            models: Arc::new(models.clone()),
            templates: Arc::new(templates.clone()),
            eligibility_nudge: nudge,
            tool_dispatch_gate: InProcessToolDispatchGate::default(),
        },
    );
    let reload = ConfigurationReload::new(
        core_pool.clone(),
        models,
        templates,
        model_path.clone(),
        template_path.clone(),
        None,
    )
    .expect("reload composition")
    .with_repository_watch(runtime.clone());
    hook.enabled = true;
    let push_credential = files.path().join("push-token");
    write_private_credential(&push_credential, b"")?;
    let replacement_source = runtime_configuration_source(&hook)?.replace(
        "credential_file =",
        &format!(
            "push_credential_file = \"{}\"\ncredential_file =",
            push_credential.display()
        ),
    );
    let replacement = signalboxd::HubModelConfiguration::parse(&replacement_source)?;
    let watch = replacement.repository_watch().expect("watch configuration");
    let sets = watch
        .repositories()
        .iter()
        .map(|repo| RepositoryRuleSet::new(repo.repository(), watch.rules()))
        .collect::<Vec<_>>();
    let digest = repository_rule_set_digest(&sets)?;
    let snapshot = |source: &str| -> Result<String, Box<dyn Error>> {
        let mut document = source.parse::<toml_edit::DocumentMut>()?;
        document.as_table_mut().retain(|key, _| {
            ["models", "serving_targets", "aliases", "repository_watch"].contains(&key)
        });
        Ok(serde_json::json!({"model_catalog":document.to_string(), "session_templates":templates_source}).to_string())
    };
    let request = ReloadConfiguration {
        command_id: signalbox_domain::DurableCommandId::from_uuid(Uuid::now_v7()),
    };
    let repository = ReloadConfigurationRepository::new(core_pool.clone());
    let intent = ReloadIntent {
        replacement_snapshot: snapshot(&replacement_source)?,
        prior_snapshot: snapshot(&prior_source)?,
        rule_set_digest: digest,
    };
    assert_eq!(
        repository.claim(request, Ok(&intent)).await?,
        ReloadClaim::Retained
    );
    let store = RepoWatchStore::new(module_pool.clone());
    let input = ReloadIntentInput {
        command_id: request.command_id,
        repositories: &sets,
        rule_set_digest: digest,
    };
    assert!(matches!(
        store
            .activate_reload(input, OffsetDateTime::now_utc())
            .await?,
        RuleReconciliationAdmission::Applied { .. }
    ));
    let first_tails: String = sqlx::query_scalar(
        "SELECT activation_tails::text FROM reload_activation WHERE command_id = $1",
    )
    .bind(request.command_id.as_uuid())
    .fetch_one(&module_pool)
    .await?;
    // No model file exists: recovery must use the checked intent after the activation commit.
    std::fs::remove_file(&template_path)?;
    reload.recover().await?;
    let recovered_catalogs = reload.catalogs();
    assert_eq!(
        recovered_catalogs
            .models
            .repository_watch()
            .expect("recovered watch")
            .repositories()[0]
            .push_credential_file(),
        Some(push_credential.as_path()),
    );
    std::fs::write(&template_path, templates_source)?;
    assert_eq!(
        repository.lookup(request).await?,
        ReloadLookup::Recorded(ReloadResult::Reloaded)
    );
    let replay_tails: String = sqlx::query_scalar(
        "SELECT activation_tails::text FROM reload_activation WHERE command_id = $1",
    )
    .bind(request.command_id.as_uuid())
    .fetch_one(&module_pool)
    .await?;
    assert_eq!(first_tails, replay_tails);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&first_tails)?,
        serde_json::json!({"runtime/project":"0"})
    );
    assert!(
        sqlx::query("DELETE FROM reload_activation")
            .execute(&module_pool)
            .await
            .is_err()
    );
    let watched_repository = RepositorySlug::try_new(String::from("runtime/project"))?;
    let obsolete_reviewers = [RepoWatchAuthorLogin::try_new(String::from(
        "obsolete-reviewer",
    ))?];
    store
        .prepare_poll_cache(&watched_repository, &obsolete_reviewers)
        .await?;
    sqlx::query("INSERT INTO poll_cache_page(repository,resource_key,etag,has_next,snapshot) VALUES ('runtime/project','/repos/runtime/project','old',false,'[\"metadata\",\"main\"]'::jsonb)")
        .execute(&module_pool).await?;
    let (shutdown, stopped) = tokio::sync::watch::channel(false);
    let worker = runtime.spawn(stopped).await;
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(hook.address).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        webhook_status(&hook, b"wrong-secret").await?,
        reqwest::StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM poll_cache_page")
            .fetch_one(&module_pool)
            .await?,
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT reviewers::text FROM poll_cache_reviewers WHERE repository='runtime/project'"
        )
        .fetch_one(&module_pool)
        .await?,
        "[]"
    );
    // Pause after delivery admission starts, while rule activation remains blocked.
    let delivery = RuntimeWebhookDelivery {
        id: Uuid::now_v7(),
        event: "push",
        body: RUNTIME_WEBHOOK_BODY,
    };
    let mut admission_lock = module_pool.begin().await?;
    sqlx::query("LOCK TABLE webhook_delivery IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *admission_lock)
        .await?;
    let mut activation_lock = module_pool.begin().await?;
    sqlx::query("LOCK TABLE reload_activation IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *activation_lock)
        .await?;
    hook.mode = "primary";
    std::fs::write(&model_path, runtime_configuration_source(&hook)?)?;
    let (response_sent, response_received) = tokio::sync::oneshot::channel();
    let control_pool = module_pool.clone();
    let delivery_id = delivery.id;
    let hook_id = hook.id;
    let (response, installed, controls) = tokio::join!(
        async {
            let response = webhook_delivery_status(&hook, b"hook-secret", &delivery).await?;
            let _ = response_sent.send(response);
            Ok::<_, Box<dyn Error>>(response)
        },
        async {
            wait_for_blocked_webhook_admission(&module_pool).await?;
            Ok::<_, Box<dyn Error>>(
                reload
                    .reload(ReloadConfiguration {
                        command_id: signalbox_domain::DurableCommandId::from_uuid(Uuid::now_v7()),
                    })
                    .await?,
            )
        },
        async move {
            wait_for_blocked_reload_activation(&control_pool).await?;
            admission_lock.commit().await?;
            assert_eq!(
                response_received.await?,
                reqwest::StatusCode::SERVICE_UNAVAILABLE
            );
            let pending: String = sqlx::query_scalar("SELECT disposition FROM webhook_disposition WHERE hook_id = $1 AND delivery_id = $2")
                .bind(Decimal::from(hook_id)).bind(delivery_id).fetch_one(&control_pool).await?;
            assert_eq!(pending, "pending");
            activation_lock.commit().await?;
            Ok::<_, Box<dyn Error>>(())
        }
    );
    controls?;
    assert_eq!(response?, reqwest::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(installed?, ReloadLookup::Recorded(ReloadResult::Reloaded));
    assert_eq!(
        webhook_delivery_status(&hook, b"hook-secret", &delivery).await?,
        reqwest::StatusCode::ACCEPTED
    );
    let applied: String = sqlx::query_scalar(
        "SELECT disposition FROM webhook_disposition WHERE hook_id = $1 AND delivery_id = $2",
    )
    .bind(Decimal::from(hook.id))
    .bind(delivery.id)
    .fetch_one(&module_pool)
    .await?;
    assert_eq!(applied, "applied");
    let occupied = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let running_address = hook.address;
    hook.address = occupied.local_addr()?;
    std::fs::write(&model_path, runtime_configuration_source(&hook)?)?;
    let bind_failure = ReloadConfiguration {
        command_id: signalbox_domain::DurableCommandId::from_uuid(Uuid::now_v7()),
    };
    assert!(matches!(
        reload.reload(bind_failure).await?,
        ReloadLookup::Recorded(ReloadResult::Failed {
            phase: ReloadPhase::Install,
            ..
        })
    ));
    hook.address = running_address;
    assert_eq!(
        webhook_status(&hook, b"wrong-secret").await?,
        reqwest::StatusCode::UNAUTHORIZED
    );
    hook.enabled = false;
    hook.rule_version = 2;
    std::fs::write(&model_path, runtime_configuration_source(&hook)?)?;
    let disable = ReloadConfiguration {
        command_id: signalbox_domain::DurableCommandId::from_uuid(Uuid::now_v7()),
    };
    let mut target_lock = core_pool.begin().await?;
    sqlx::query("LOCK TABLE convergence_sweep_target IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *target_lock)
        .await?;
    let (disabled, published_before_restoration) = tokio::join!(reload.reload(disable), async {
        wait_for_blocked_target_reconciliation(&core_pool).await?;
        let published = !reload
            .catalogs()
            .models
            .repository_watch()
            .expect("watch")
            .enabled();
        target_lock.commit().await?;
        Ok::<_, Box<dyn Error>>(published)
    });
    assert!(
        published_before_restoration?,
        "replacement catalogs precede eligibility restoration"
    );
    assert_eq!(disabled?, ReloadLookup::Recorded(ReloadResult::Reloaded));
    assert!(tokio::net::TcpStream::connect(hook.address).await.is_err());
    hook.enabled = true;
    hook.rule_version = 1;
    std::fs::write(&model_path, runtime_configuration_source(&hook)?)?;
    let stale = ReloadConfiguration {
        command_id: signalbox_domain::DurableCommandId::from_uuid(Uuid::now_v7()),
    };
    assert!(matches!(
        reload.reload(stale).await?,
        ReloadLookup::Recorded(ReloadResult::Failed {
            phase: ReloadPhase::Activate,
            ..
        })
    ));
    assert!(
        !reload
            .catalogs()
            .models
            .repository_watch()
            .expect("prior watch")
            .enabled()
    );
    assert!(tokio::net::TcpStream::connect(hook.address).await.is_err());
    assert!(matches!(
        store
            .activate_reload(input, OffsetDateTime::now_utc())
            .await?,
        RuleReconciliationAdmission::Applied { .. }
    ));
    let active_count: i64 = sqlx::query_scalar("SELECT count(*) FROM rule")
        .fetch_one(&module_pool)
        .await?;
    assert_eq!(
        active_count, 0,
        "replaying an older activation cannot undo a later disable"
    );
    store
        .prepare_poll_cache(&watched_repository, &obsolete_reviewers)
        .await?;
    sqlx::query("INSERT INTO poll_cache_page(repository,resource_key,etag,has_next,snapshot) VALUES ('runtime/project','/repos/runtime/project','old',false,'[\"metadata\",\"main\"]'::jsonb)")
        .execute(&module_pool).await?;
    hook.enabled = true;
    hook.rule_version = 2;
    std::fs::write(&model_path, runtime_configuration_source(&hook)?)?;
    let reenable = ReloadConfiguration {
        command_id: signalbox_domain::DurableCommandId::from_uuid(Uuid::now_v7()),
    };
    assert_eq!(
        reload.reload(reenable).await?,
        ReloadLookup::Recorded(ReloadResult::Reloaded)
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM poll_cache_page")
            .fetch_one(&module_pool)
            .await?,
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT reviewers::text FROM poll_cache_reviewers WHERE repository='runtime/project'"
        )
        .fetch_one(&module_pool)
        .await?,
        "[]"
    );
    shutdown.send(true)?;
    worker.await?.expect("clean worker shutdown");
    Ok(())
}

#[derive(Clone)]
struct ConditionalRequest {
    path: String,
    etag: Option<String>,
    last_modified: Option<String>,
}

struct ConditionalPollFixture {
    pages: std::collections::BTreeMap<String, (serde_json::Value, bool)>,
    requests: std::sync::Mutex<Vec<ConditionalRequest>>,
    changed: bool,
}

impl ConditionalPollFixture {
    fn new() -> Self {
        use serde_json::json;
        // Provider numbers, revisions, and validator text are arbitrary fixture identities.
        let root = "/repos/example/project";
        let head = "1111111111111111111111111111111111111111";
        let pages = [
            (root.to_owned(), json!({"default_branch":"main","private_provider_field":"discard-me"}),false),
            (format!("{root}/branches?per_page=100&page=1"),json!([{"name":"main","commit":{"sha":head}}]),false),
            (format!("{root}/pulls?state=open&per_page=100&page=1"),json!([]),true),
            (format!("{root}/pulls?state=open&per_page=100&page=2"),json!([{"number":1}]),false),
            (format!("{root}/pulls/1"),json!({"number":1,"title":"change","body":null,"draft":false,"user":{"login":"author"},"labels":[],"state":"open","merged_at":null,"mergeable":true,"head":{"sha":head,"ref":"topic","repo":{"full_name":"example/project"}},"base":{"ref":"main"}}),false),
            (format!("{root}/commits/{head}/check-suites?filter=all&per_page=100&page=1"),json!({"check_suites":[{"id":2,"status":"completed","updated_at":"generation","conclusion":"success"},{"id":3,"status":"pending"}]}),false),
            (format!("{root}/commits/{head}/check-runs?filter=all&per_page=100&page=1"),json!({"check_runs":[{"id":4,"status":"completed","completed_at":"generation","name":"test","conclusion":"success"}]}),false),
            (format!("{root}/pulls/1/reviews?per_page=100&page=1"),json!([{"id":5,"user":{"login":"reviewer"},"state":"APPROVED","commit_id":head},{"state":"PENDING"}]),false),
            (format!("{root}/issues/1/comments?per_page=100&page=1"),json!([{"id":6,"body":"discard-me"}]),false),
            (format!("{root}/pulls/1/comments?per_page=100&page=1"),json!([{"id":7,"body":"discard-me"}]),false),
            (format!("{root}/issues/1/reactions?per_page=100&page=1"),json!([{"user":{"login":"reviewer"},"content":"+1"},{"user":{"login":"outsider"},"content":"-1"}]),false),
            (format!("{root}/issues/comments/6/reactions?per_page=100&page=1"),json!([{"user":{"login":"reviewer"},"content":"heart"}]),false),
            (format!("{root}/pulls/comments/7/reactions?per_page=100&page=1"),json!([{"user":{"login":"reviewer"},"content":"eyes"}]),false),
            (format!("{root}/actions/runs?head_sha={head}&status=completed&per_page=100&page=1"),json!({"total_count":1,"workflow_runs":[{"id":8,"workflow_id":9,"run_attempt":1,"head_branch":"main","head_repository":{"full_name":"example/project"},"name":"build","conclusion":"success","status":"completed"}]}),false),
        ].into_iter().map(|(path,value,next)| (path,(value,next))).collect();
        Self {
            pages,
            requests: std::sync::Mutex::new(Vec::new()),
            changed: false,
        }
    }
}

impl signalbox_module_repo_watch_v2::poll_cache::ConditionalObservationRead
    for ConditionalPollFixture
{
    async fn conditional_page(
        &self,
        path: &str,
        validators: Option<&signalbox_module_repo_watch_v2::github::HttpValidators>,
    ) -> Result<
        signalbox_module_repo_watch_v2::github::ConditionalPage,
        signalbox_module_repo_watch_v2::provider::ObservationError,
    > {
        use signalbox_module_repo_watch_v2::{
            github::{ConditionalPage, HttpValidators},
            provider::ObservationError,
        };
        self.requests
            .lock()
            .expect("request log")
            .push(ConditionalRequest {
                path: path.to_owned(),
                etag: validators.and_then(|v| v.etag.clone()),
                last_modified: validators.and_then(|v| v.last_modified.clone()),
            });
        let (value, next) = self
            .pages
            .get(path)
            .ok_or(ObservationError::InvalidResponse)?;
        if validators.is_some() && !self.changed {
            return Ok(ConditionalPage::Unchanged);
        }
        Ok(ConditionalPage::Modified {
            body: value.to_string().into_bytes(),
            has_next: *next,
            validators: HttpValidators {
                etag: Some(String::from("\"fixture-etag\"")),
                last_modified: Some(String::from("Mon, 07 Sep 2026 12:00:00 GMT")),
            },
        })
    }
    async fn threads(
        &self,
        _: serde_json::Value,
    ) -> Result<serde_json::Value, signalbox_module_repo_watch_v2::provider::ObservationError> {
        Ok(
            serde_json::json!({"data":{"repository":{"pullRequest":{"reviewThreads":{"nodes":[],"pageInfo":{"hasNextPage":false}}}}}}),
        )
    }
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn undated_compact_entries_do_not_reopen_ordinary_pull_requests() -> Result<(), Box<dyn Error>>
{
    use signalbox_module_repo_watch_v2::poll_cache::poll_with_cache;
    let (container, core_pool, url) = postgres().await?;
    migrate(&core_pool).await?;
    sqlx::query("ALTER ROLE mod_repo_watch PASSWORD 'signalbox-test-only'")
        .execute(&core_pool)
        .await?;
    let pool = module_pool(&url).await?;
    let store = RepoWatchStore::new(pool.clone());
    let repository = RepositorySlug::try_new(String::from("example/project"))?;
    store.prepare_poll_cache(&repository, &[]).await?;
    let io = ConditionalPollFixture::new();
    poll_with_cache(
        &io,
        &store,
        &repository,
        &[],
        EventProducer::Poll,
        MERGED_RETENTION,
    )
    .await?;
    let initial = store.ingest_baseline(&repository).await?;
    let head = initial
        .observation
        .as_ref()
        .expect("ordinary predecessor")
        .state()
        .pull_requests()[0]
        .context()
        .head_sha();
    // The two stored compact subjects are distinct from the fixture's open PR.
    let undated = serde_json::json!({
        "number": 2, "head_repository": repository.as_str(), "head_sha": head.as_str(),
        "signal_reviewers": [], "labels": [], "mergeable_state": "unknown",
        "completed_check_suites": [], "completed_check_runs": [], "review_ids": [],
        "threads": [], "reactions": []
    });
    let mut dated = undated.clone();
    dated["number"] = serde_json::json!(3);
    let merged_at = OffsetDateTime::now_utc().unix_timestamp();
    dated["merged_at"] = serde_json::json!(merged_at);
    sqlx::query("UPDATE repository_state SET comparison_baseline = jsonb_set(comparison_baseline, '{merged_pull_requests}', $2::jsonb) WHERE repository = $1")
        .bind(repository.as_str())
        .bind(serde_json::json!([undated, dated]).to_string())
        .execute(&pool).await?;
    let reopened = RepoWatchStore::new(pool.clone());
    let predecessor = reopened.ingest_baseline(&repository).await?;
    assert_eq!(predecessor.observation, initial.observation);
    assert_eq!(predecessor.frontier, initial.frontier);
    assert_eq!(predecessor.generation, initial.generation);
    assert_eq!(predecessor.merged_baselines.len(), 1);
    assert_eq!(predecessor.merged_baselines[0].state.number().get(), 3);
    assert_eq!(
        predecessor.merged_baselines[0].merged_at.unix_timestamp(),
        merged_at
    );
    let opened_before: i64 = sqlx::query_scalar("SELECT count(*) FROM gh_event WHERE repository = $1 AND event_kind = 'pull_request_opened'")
        .bind(repository.as_str()).fetch_one(&pool).await?;
    assert!(matches!(
        poll_with_cache(&io, &reopened, &repository, &[], EventProducer::Poll, MERGED_RETENTION).await?,
        FrontierEventAdmission::Committed { events, .. } if events.is_empty()
    ));
    let opened_after: i64 = sqlx::query_scalar("SELECT count(*) FROM gh_event WHERE repository = $1 AND event_kind = 'pull_request_opened'")
        .bind(repository.as_str()).fetch_one(&pool).await?;
    assert_eq!(
        opened_after, opened_before,
        "recovery does not emit another opened event"
    );
    let retained = reopened.ingest_baseline(&repository).await?;
    assert_eq!(retained.observation, initial.observation);
    assert_eq!(retained.merged_baselines, predecessor.merged_baselines);
    let stored_count: i32 = sqlx::query_scalar("SELECT jsonb_array_length(comparison_baseline->'merged_pull_requests') FROM repository_state WHERE repository = $1")
        .bind(repository.as_str()).fetch_one(&pool).await?;
    assert_eq!(
        stored_count, 1,
        "the next commit removes only the undated compact entry"
    );
    pool.close().await;
    core_pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn restart_reuses_every_persisted_page_and_reviewer_change_invalidates_the_cache()
-> Result<(), Box<dyn Error>> {
    use signalbox_module_repo_watch_v2::poll_cache::poll_with_cache;
    let (container, core_pool, url) = unmigrated_postgres().await?;
    migrate(&core_pool).await?;
    sqlx::query("ALTER ROLE mod_repo_watch PASSWORD 'signalbox-test-only'")
        .execute(&core_pool)
        .await?;
    let pool = module_pool(&url).await?;
    let store = RepoWatchStore::new(pool.clone());
    let repository = RepositorySlug::try_new(String::from("example/project"))?;
    let reviewers = [RepoWatchAuthorLogin::try_new(String::from("reviewer"))?];
    store.prepare_poll_cache(&repository, &reviewers).await?;
    let first = ConditionalPollFixture::new();
    assert!(matches!(
        poll_with_cache(
            &first,
            &store,
            &repository,
            &reviewers,
            EventProducer::Poll,
            MERGED_RETENTION
        )
        .await?,
        FrontierEventAdmission::Committed { .. }
    ));
    let original = store.ingest_baseline(&repository).await?.observation;
    let snapshots: Vec<String> =
        sqlx::query_scalar("SELECT snapshot::text FROM poll_cache_page ORDER BY resource_key")
            .fetch_all(&pool)
            .await?;
    assert_eq!(snapshots.len(), first.pages.len());
    assert!(!snapshots.join(" ").contains("discard-me"));
    assert!(!snapshots.join(" ").contains("outsider"));
    drop(store);
    pool.close().await;
    let restarted_pool = module_pool(&url).await?;
    let restarted = RepoWatchStore::new(restarted_pool.clone());
    let same_set_different_case = [RepoWatchAuthorLogin::try_new(String::from("REVIEWER"))?];
    restarted
        .prepare_poll_cache(&repository, &same_set_different_case)
        .await?;
    let second = ConditionalPollFixture::new();
    assert!(matches!(
        poll_with_cache(
            &second,
            &restarted,
            &repository,
            &reviewers,
            EventProducer::Poll, MERGED_RETENTION)
        .await?,
        FrontierEventAdmission::Committed { events, .. } if events.is_empty()
    ));
    assert_eq!(
        restarted.ingest_baseline(&repository).await?.observation,
        original
    );
    let requests = second.requests.lock().expect("request log").clone();
    assert_eq!(
        requests
            .iter()
            .map(|request| request.path.as_str())
            .collect::<std::collections::BTreeSet<_>>(),
        second.pages.keys().map(String::as_str).collect()
    );
    assert!(requests.iter().all(
        |ConditionalRequest {
             etag,
             last_modified: modified,
             ..
         }| etag.as_deref() == Some("\"fixture-etag\"")
            && modified.as_deref() == Some("Mon, 07 Sep 2026 12:00:00 GMT")
    ));
    let replacement_reviewers = [RepoWatchAuthorLogin::try_new(String::from("outsider"))?];
    restarted
        .prepare_poll_cache(&repository, &replacement_reviewers)
        .await?;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM poll_cache_page")
            .fetch_one(&restarted_pool)
            .await?,
        0
    );
    let third = ConditionalPollFixture::new();
    assert!(matches!(
        poll_with_cache(
            &third,
            &restarted,
            &repository,
            &replacement_reviewers,
            EventProducer::Poll,
            MERGED_RETENTION
        )
        .await?,
        FrontierEventAdmission::Committed { .. }
    ));
    assert!(third.requests.lock().expect("request log").iter().all(
        |ConditionalRequest {
             etag,
             last_modified: modified,
             ..
         }| etag.is_none() && modified.is_none()
    ));
    let reaction_actors: Vec<String> = sqlx::query_scalar("SELECT DISTINCT item->>'reviewer' FROM poll_cache_page CROSS JOIN LATERAL jsonb_array_elements(snapshot->1) item WHERE resource_key LIKE '%/reactions?%'")
        .fetch_all(&restarted_pool).await?;
    assert_eq!(
        reaction_actors,
        vec![replacement_reviewers[0].as_str().to_owned()]
    );
    restarted_pool.close().await;
    core_pool.close().await;
    container.stop().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn completed_polls_prune_terminal_and_expired_subject_pages() -> Result<(), Box<dyn Error>> {
    use serde_json::json;
    use signalbox_module_repo_watch_v2::poll_cache::poll_with_cache;
    let (container, core_pool, url) = postgres().await?;
    migrate(&core_pool).await?;
    sqlx::query("ALTER ROLE mod_repo_watch PASSWORD 'signalbox-test-only'")
        .execute(&core_pool)
        .await?;
    let pool = module_pool(&url).await?;
    let store = RepoWatchStore::new(pool.clone());
    let repository = RepositorySlug::try_new(String::from("example/project"))?;
    let reviewers = [RepoWatchAuthorLogin::try_new(String::from("reviewer"))?];
    store.prepare_poll_cache(&repository, &reviewers).await?;
    let root = "/repos/example/project";
    let pull_path = format!("{root}/pulls/1");
    let head = "1111111111111111111111111111111111111111";
    let workflow_path =
        format!("{root}/actions/runs?head_sha={head}&status=completed&per_page=100&page=1");
    for merged in [false, true] {
        let mut open = ConditionalPollFixture::new();
        open.changed = true;
        poll_with_cache(
            &open,
            &store,
            &repository,
            &reviewers,
            EventProducer::Poll,
            MERGED_RETENTION,
        )
        .await?;
        let initial_keys: Vec<String> =
            sqlx::query_scalar("SELECT resource_key FROM poll_cache_page ORDER BY resource_key")
                .fetch_all(&pool)
                .await?;
        assert_eq!(initial_keys.len(), open.pages.len());
        let stale_pull: String = sqlx::query_scalar(
            "SELECT snapshot::text FROM poll_cache_page WHERE resource_key = $1",
        )
        .bind(&pull_path)
        .fetch_one(&pool)
        .await?;
        let mut incomplete = ConditionalPollFixture::new();
        incomplete.pages.remove(&workflow_path);
        assert!(
            poll_with_cache(
                &incomplete,
                &store,
                &repository,
                &reviewers,
                EventProducer::Poll,
                MERGED_RETENTION
            )
            .await
            .is_err()
        );
        let after_failure: Vec<String> =
            sqlx::query_scalar("SELECT resource_key FROM poll_cache_page ORDER BY resource_key")
                .fetch_all(&pool)
                .await?;
        assert_eq!(
            after_failure, initial_keys,
            "a failed poll cannot prune accepted pages"
        );

        let mut terminal = ConditionalPollFixture::new();
        terminal.changed = true;
        terminal
            .pages
            .get_mut(&format!("{root}/pulls?state=open&per_page=100&page=2"))
            .expect("open page")
            .0 = json!([]);
        let detail = &mut terminal.pages.get_mut(&pull_path).expect("pull detail").0;
        detail["state"] = json!("closed");
        if merged {
            let now = OffsetDateTime::now_utc();
            detail["merged_at"] = json!(format!(
                "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
                now.year(),
                u8::from(now.month()),
                now.day(),
                now.hour(),
                now.minute(),
                now.second()
            ));
        }
        poll_with_cache(
            &terminal,
            &store,
            &repository,
            &reviewers,
            EventProducer::Poll,
            MERGED_RETENTION,
        )
        .await?;
        let expected = std::collections::BTreeSet::from([
            root.to_owned(),
            format!("{root}/branches?per_page=100&page=1"),
            format!("{root}/pulls?state=open&per_page=100&page=1"),
            format!("{root}/pulls?state=open&per_page=100&page=2"),
            workflow_path.clone(),
        ]);
        let keys: Vec<String> = sqlx::query_scalar("SELECT resource_key FROM poll_cache_page")
            .fetch_all(&pool)
            .await?;
        assert_eq!(
            keys.into_iter().collect::<std::collections::BTreeSet<_>>(),
            expected,
            "terminal PR details, checks, reviews, comments and reactions leave the cache while the shared default-head workflow stays"
        );
        if merged {
            assert_eq!(
                store
                    .ingest_baseline(&repository)
                    .await?
                    .merged_baselines
                    .len(),
                1
            );
            sqlx::query("INSERT INTO poll_cache_page(repository,resource_key,etag,has_next,snapshot) VALUES ($1,$2,'stale',false,$3::jsonb)")
                .bind(repository.as_str()).bind(&pull_path).bind(stale_pull).execute(&pool).await?;
            let expired = OffsetDateTime::now_utc() - MERGED_RETENTION - Duration::from_secs(1);
            sqlx::query("UPDATE repository_state SET comparison_baseline=jsonb_set(comparison_baseline,'{merged_pull_requests,0,merged_at}',to_jsonb($2::bigint)) WHERE repository=$1")
                .bind(repository.as_str()).bind(expired.unix_timestamp()).execute(&pool).await?;
            terminal.changed = false;
            poll_with_cache(
                &terminal,
                &store,
                &repository,
                &reviewers,
                EventProducer::Poll,
                MERGED_RETENTION,
            )
            .await?;
            assert!(
                store
                    .ingest_baseline(&repository)
                    .await?
                    .merged_baselines
                    .is_empty()
            );
            let keys: Vec<String> = sqlx::query_scalar("SELECT resource_key FROM poll_cache_page")
                .fetch_all(&pool)
                .await?;
            assert_eq!(
                keys.into_iter().collect::<std::collections::BTreeSet<_>>(),
                expected,
                "expiry prunes stale compact-subject pages while preserving unchanged live pages"
            );
        }
    }
    pool.close().await;
    core_pool.close().await;
    drop(container);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn incomplete_poll_does_not_retain_validators_from_partial_pages()
-> Result<(), Box<dyn Error>> {
    use signalbox_module_repo_watch_v2::poll_cache::poll_with_cache;
    let (container, core_pool, url) = unmigrated_postgres().await?;
    migrate(&core_pool).await?;
    sqlx::query("ALTER ROLE mod_repo_watch PASSWORD 'signalbox-test-only'")
        .execute(&core_pool)
        .await?;
    let pool = module_pool(&url).await?;
    let store = RepoWatchStore::new(pool.clone());
    let repository = RepositorySlug::try_new(String::from("example/project"))?;
    let reviewers = [RepoWatchAuthorLogin::try_new(String::from("reviewer"))?];
    store.prepare_poll_cache(&repository, &reviewers).await?;
    let mut incomplete = ConditionalPollFixture::new();
    incomplete.pages.remove("/repos/example/project/pulls/1");
    assert!(
        poll_with_cache(
            &incomplete,
            &store,
            &repository,
            &reviewers,
            EventProducer::Poll,
            MERGED_RETENTION
        )
        .await
        .is_err()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM poll_cache_page")
            .fetch_one(&pool)
            .await?,
        0
    );
    assert!(
        store
            .ingest_baseline(&repository)
            .await?
            .observation
            .is_none()
    );
    let complete = ConditionalPollFixture::new();
    assert!(matches!(
        poll_with_cache(
            &complete,
            &store,
            &repository,
            &reviewers,
            EventProducer::Poll,
            MERGED_RETENTION
        )
        .await?,
        FrontierEventAdmission::Committed { .. }
    ));
    assert!(complete.requests.lock().expect("request log").iter().all(
        |ConditionalRequest {
             etag,
             last_modified: modified,
             ..
         }| etag.is_none() && modified.is_none()
    ));
    pool.close().await;
    core_pool.close().await;
    container.stop().await?;
    Ok(())
}
