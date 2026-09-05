#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "the standalone integration test uses assertion panics and explicit fixture expectations"
)]

use std::{error::Error, num::NonZeroU64, time::Duration};

use rust_decimal::Decimal;
use signalbox_module_repo_watch_v2::{
    EventAdmission, FrontierEventAdmission, PullRequestLifecycle, PullRequestState, RepoWatchStore,
    RepositoryState, RuleAdmission, WebhookAdmission, WebhookDelivery, WebhookDisposition,
    matching_rules,
};
use signalbox_ownership_seam::{
    BranchName, CommitSha, OffsetDateTime, PullRequestBody, PullRequestNumber, PullRequestTitle,
    RepoWatchAuthorLogin, RepoWatchEvent, RepoWatchEventContentIdentityV1, RepoWatchEventId,
    RepoWatchEventIdentityFrontierEntryV1, RepoWatchEventIdentityFrontierV1,
    RepoWatchEventKindNameV1, RepoWatchEventOccurrenceV1, RepoWatchLabelMatcher,
    RepoWatchMatcherV1, RepoWatchMatcherV1Input, RepoWatchRule, RepoWatchRuleActionV1,
    RepoWatchRuleId, RepoWatchRuleVersion, RepoWatchSingletonScope, RepositorySlug,
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
    let options = local_test_connection_options(database_url)?;
    PgPoolOptions::new()
        .max_connections(2)
        .after_connect(|connection, _metadata| {
            Box::pin(async move {
                sqlx::query("SET ROLE mod_repo_watch")
                    .execute(&mut *connection)
                    .await?;
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
    let module_pool = module_pool(&database_url).await?;
    let store = RepoWatchStore::new(module_pool.clone());

    let repository = RepositorySlug::try_new(String::from("owner/repository"))?;
    let default_branch = BranchName::try_new(String::from("main"))?;
    let default_head =
        CommitSha::try_new(String::from("1111111111111111111111111111111111111111"))?;
    let observed_at = OffsetDateTime::UNIX_EPOCH + Duration::from_secs(1_000);
    let body = br#"{"action":"opened"}"#;
    let delivery = || WebhookDelivery {
        repository: &repository,
        hook_id: 9,
        delivery_id: Uuid::from_u128(10),
        event: "pull_request",
        action: Some("opened"),
        body_digest: [11; 32],
        body,
        received_at: observed_at,
        expires_at: observed_at + Duration::from_secs(60),
    };
    assert_eq!(
        store.admit_webhook(delivery()).await?,
        WebhookAdmission::Inserted
    );

    store
        .upsert_repository(RepositoryState {
            repository: &repository,
            default_branch: &default_branch,
            default_head: &default_head,
            observed_at,
        })
        .await?;

    let title = PullRequestTitle::try_new(String::from("A bounded rewrite"))?;
    let body = PullRequestBody::try_new(String::from("Current provider state"))?;
    let author = RepoWatchAuthorLogin::try_new(String::from("octocat"))?;
    store
        .upsert_pull_request(PullRequestState {
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
        })
        .await?;

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
    let (first, concurrent) = tokio::join!(
        store.record_rule(&repository, &rule, observed_at),
        store.record_rule(&repository, &rule, observed_at)
    );
    assert!(matches!(
        (first?, concurrent?),
        (RuleAdmission::Inserted, RuleAdmission::Replayed)
            | (RuleAdmission::Replayed, RuleAdmission::Inserted)
    ));
    assert_eq!(
        store.record_rule(&repository, &rule, observed_at).await?,
        RuleAdmission::Replayed
    );
    let fingerprint_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM mod_repo_watch.rule_field_fingerprint
          WHERE repository = $1 AND rule_id = $2",
    )
    .bind(repository.as_str())
    .bind(rule.id().as_str())
    .fetch_one(&core_pool)
    .await?;
    assert_eq!(
        usize::try_from(fingerprint_count)?,
        rule.identity_field_digests().len()
    );
    let other_repository = RepositorySlug::try_new(String::from("other/repository"))?;
    assert_eq!(
        store
            .record_rule(&other_repository, &rule, observed_at)
            .await?,
        RuleAdmission::Inserted
    );
    assert!(
        store
            .deactivate_rule(&other_repository, rule.id().as_str(), observed_at)
            .await?
    );
    let retained_revisions: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM mod_repo_watch.rule_revision
          WHERE repository = $1 AND rule_id = $2",
    )
    .bind(other_repository.as_str())
    .bind(rule.id().as_str())
    .fetch_one(&core_pool)
    .await?;
    assert_eq!(retained_revisions, 1);

    let event = RepoWatchEvent::branch_workflow(
        RepoWatchEventId::from_uuid(Uuid::from_u128(14)),
        repository.clone(),
        default_branch.clone(),
        WorkflowName::try_new(String::from("ci"))?,
        signalbox_ownership_seam::CheckConclusion::Success,
    );
    assert_eq!(matching_rules(std::slice::from_ref(&rule), &event), [&rule]);
    let identity = RepoWatchEventContentIdentityV1::from_bytes([15; 32]);
    let occurrence = RepoWatchEventOccurrenceV1::from_parts(event.clone(), identity);
    let retain_until = observed_at + Duration::from_secs(120);
    assert_eq!(
        store
            .commit_frontier_candidate(
                &repository,
                0,
                &frontier,
                std::slice::from_ref(&occurrence),
                observed_at,
                retain_until,
            )
            .await?,
        FrontierEventAdmission::Committed {
            generation: 1,
            events: Box::new([EventAdmission::Inserted]),
        }
    );
    let replayed_event = RepoWatchEvent::branch_workflow(
        RepoWatchEventId::from_uuid(Uuid::from_u128(16)),
        repository.clone(),
        default_branch.clone(),
        WorkflowName::try_new(String::from("ci"))?,
        signalbox_ownership_seam::CheckConclusion::Success,
    );
    let replayed_occurrence = RepoWatchEventOccurrenceV1::from_parts(replayed_event, identity);
    assert_eq!(
        store
            .commit_frontier_candidate(
                &repository,
                0,
                &frontier,
                std::slice::from_ref(&replayed_occurrence),
                observed_at + Duration::from_secs(1),
                retain_until + Duration::from_secs(1),
            )
            .await?,
        FrontierEventAdmission::Committed {
            generation: 1,
            events: Box::new([EventAdmission::Replayed]),
        }
    );
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
                &repository,
                0,
                &incompatible_frontier,
                &[],
                observed_at,
                retain_until,
            )
            .await?,
        FrontierEventAdmission::Stale
    );
    let payload: Vec<u8> = sqlx::query_scalar(
        "SELECT normalized_payload FROM mod_repo_watch.gh_event WHERE event_id = $1",
    )
    .bind(event.id().into_uuid())
    .fetch_one(&core_pool)
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
    let preceding_occurrence = RepoWatchEventOccurrenceV1::from_parts(
        preceding_event.clone(),
        RepoWatchEventContentIdentityV1::from_bytes([17; 32]),
    );
    let conflicting_occurrence = RepoWatchEventOccurrenceV1::from_parts(
        event.clone(),
        RepoWatchEventContentIdentityV1::from_bytes([16; 32]),
    );
    assert_eq!(
        store
            .commit_frontier_candidate(
                &repository,
                1,
                &next_frontier,
                &[preceding_occurrence, conflicting_occurrence],
                observed_at,
                retain_until,
            )
            .await?,
        FrontierEventAdmission::ConflictingReuse
    );
    let preceding_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM mod_repo_watch.gh_event WHERE event_id = $1")
            .bind(preceding_event.id().into_uuid())
            .fetch_one(&core_pool)
            .await?;
    assert_eq!(preceding_count, 0);
    let frontier_sequence: Decimal = sqlx::query_scalar(
        "SELECT sequence FROM mod_repo_watch.frontier
          WHERE repository = $1 AND stream_identity = $2",
    )
    .bind(repository.as_str())
    .bind(stream.as_slice())
    .fetch_one(&core_pool)
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
                &repository,
                1,
                &stale_frontier,
                &[],
                observed_at,
                retain_until,
            )
            .await?,
        FrontierEventAdmission::Stale
    );
    assert!(store.release_frontier(&repository, &stream).await?);
    assert!(!store.release_frontier(&repository, &stream).await?);

    let mut replay = delivery();
    replay.received_at += Duration::from_secs(1);
    replay.expires_at += Duration::from_secs(1);
    assert_eq!(
        store.admit_webhook(replay).await?,
        WebhookAdmission::Replayed
    );

    let mut conflict = delivery();
    conflict.body_digest = [12; 32];
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
