#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "the standalone integration test uses assertion panics and explicit fixture expectations"
)]

use std::{error::Error, num::NonZeroU64, time::Duration};

use rust_decimal::Decimal;
use sha2::{Digest, Sha256};
use signalbox_module_repo_watch_v2::{
    EventAdmission, EventCandidate, EventProducer, FrontierEventAdmission,
    FrontierReleaseAdmission, PullRequestLifecycle, PullRequestState, RepoWatchStore,
    RepositoryProjection, RepositoryRuleSet, RepositoryState, RuleAdmission,
    RuleReconciliationAdmission, WebhookAdmission, WebhookDelivery, WebhookDisposition,
    matching_rules,
};
use signalbox_ownership_seam::{
    BranchName, CheckConclusion, CheckRunName, ChecksOutcome, CommitSha, GitHubObjectId, LabelName,
    MergeableState, OffsetDateTime, PullRequestBody, PullRequestEventContext,
    PullRequestEventContextInput, PullRequestNumber, PullRequestTitle, ReactionContent,
    ReactionSubject, RepoWatchAuthorLogin, RepoWatchBranchHead, RepoWatchCheckCompletionGeneration,
    RepoWatchCheckRunObservation, RepoWatchCheckSuiteObservation, RepoWatchEvent,
    RepoWatchEventContentIdentityV1, RepoWatchEventId, RepoWatchEventIdentityFrontierEntryV1,
    RepoWatchEventIdentityFrontierV1, RepoWatchEventKindNameV1, RepoWatchLabelMatcher,
    RepoWatchMatcherV1, RepoWatchMatcherV1Input, RepoWatchObservation,
    RepoWatchPullRequestLifecycle, RepoWatchPullRequestState as ComparisonPullRequestState,
    RepoWatchPullRequestStateInput, RepoWatchReactionObservation, RepoWatchRepositoryState,
    RepoWatchRepositoryStateInput, RepoWatchReviewObservation, RepoWatchRule,
    RepoWatchRuleActionV1, RepoWatchRuleId, RepoWatchRuleVersion, RepoWatchSingletonScope,
    RepoWatchThreadObservation, RepoWatchThreadState, RepoWatchWorkflowRunAttempt,
    RepoWatchWorkflowRunObservation, RepositorySlug, ReviewState, ReviewThreadId,
    SessionTemplateName, WorkflowName,
};
use signalbox_persistence::{
    disposable_postgres_server_args, disposable_postgres_state_tmpfs_from_example,
    disposable_test_container_labels, local_test_connection_options, migrate,
};
use sqlx::{PgPool, postgres::PgPoolOptions};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
};
use uuid::Uuid;

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

async fn postgres() -> Result<(ContainerAsync<Postgres>, PgPool, String), Box<dyn Error>> {
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
        vec![RepoWatchRuleActionV1::DispatchSession {
            template: SessionTemplateName::try_new(String::from("repo-watch"))?,
        }],
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
        signalbox_ownership_seam::CheckConclusion::Success,
    );
    assert_eq!(matching_rules(std::slice::from_ref(&rule), &event), [&rule]);
    let identity = RepoWatchEventContentIdentityV1::from_bytes([15; 32]);
    let occurrence = event_candidate(&event, identity);
    let earlier_event = RepoWatchEvent::branch_workflow(
        RepoWatchEventId::from_uuid(Uuid::from_u128(13)),
        repository.clone(),
        default_branch.clone(),
        WorkflowName::try_new(String::from("build"))?,
        signalbox_ownership_seam::CheckConclusion::Success,
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
    let evaluation_order = store.event_evaluation_order(&repository).await?;
    assert_eq!(evaluation_order.len(), 2);
    assert_eq!(evaluation_order[0].event(), earlier_event.id());
    assert_eq!(evaluation_order[0].repository_event_ordinal(), 1);
    assert_eq!(evaluation_order[0].frontier_generation(), 1);
    assert_eq!(evaluation_order[0].event_ordinal(), 1);
    assert_eq!(evaluation_order[1].event(), event.id());
    assert_eq!(evaluation_order[1].repository_event_ordinal(), 2);
    assert_eq!(evaluation_order[1].frontier_generation(), 1);
    assert_eq!(evaluation_order[1].event_ordinal(), 2);
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
        signalbox_ownership_seam::CheckConclusion::Success,
    );
    let replayed_occurrence = event_candidate(&replayed_event, identity);
    let replayed_earlier_event = RepoWatchEvent::branch_workflow(
        RepoWatchEventId::from_uuid(Uuid::from_u128(18)),
        repository.clone(),
        default_branch.clone(),
        WorkflowName::try_new(String::from("build"))?,
        signalbox_ownership_seam::CheckConclusion::Success,
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
        signalbox_ownership_seam::CheckConclusion::Success,
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
        signalbox_ownership_seam::CheckConclusion::Success,
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
        signalbox_ownership_seam::CheckConclusion::Success,
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
        WebhookAdmission::Replayed
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

    module_pool.close().await;
    core_pool.close().await;
    drop(container);
    Ok(())
}
