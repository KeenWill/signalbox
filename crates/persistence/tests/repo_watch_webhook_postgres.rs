#![cfg(feature = "postgres-integration")]
#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

use std::{
    error::Error,
    num::{NonZeroU16, NonZeroU64},
};

use rust_decimal::Decimal;
use signalbox_application::{
    RepoWatchBranchHead, RepoWatchConvergenceAssessment, RepoWatchConvergenceAssessmentInput,
    RepoWatchEventContentIdentityV1, RepoWatchEventOccurrenceV1, RepoWatchObservation,
    RepoWatchPullRequestLifecycle, RepoWatchPullRequestState, RepoWatchPullRequestStateInput,
    RepoWatchRepositoryState, RepoWatchRepositoryStateInput, RepoWatchReviewDecision,
};
use signalbox_domain::{
    BranchName, CheckConclusion, CommitSha, MergeableState, PullRequestBody,
    PullRequestEventContext, PullRequestEventContextInput, PullRequestNumber, PullRequestTitle,
    RepoWatchAuthorLogin, RepoWatchEvent, RepoWatchEventId, RepoWatchEventKindNameV1,
    RepositorySlug, WorkflowName,
};
use signalbox_persistence::{
    disposable_test_container_labels, local_test_connection_options, migrate,
    repo_watch::{
        PostgresRepoWatchStore, RepoWatchCommitOutcome, RepoWatchCommitRequest, RepoWatchCursor,
        RepoWatchCursorCandidate, RepoWatchStoreError,
    },
    repo_watch_webhook::{
        PostgresRepoWatchWebhookStore, RepoWatchWebhookAdmission, RepoWatchWebhookAdmissionOutcome,
        RepoWatchWebhookDeliveryKey, RepoWatchWebhookDisposition, RepoWatchWebhookPendingPageSize,
        RepoWatchWebhookProjection, RepoWatchWebhookStoreError, RepoWatchWebhookTargetedQuery,
        RepoWatchWebhookTerminalOutcome, RepoWatchWebhookTerminalRequest,
    },
};
use sqlx::{
    PgPool,
    postgres::PgPoolOptions,
    types::{Uuid, time::OffsetDateTime},
};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
};

const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
const DATABASE_NAME: &str = "signalbox_repo_watch_webhook";
const DATABASE_USER: &str = "signalbox";
const DATABASE_PASSWORD: &str = "signalbox-test-only";
const REPOSITORY: &str = "signalbox/repository";
const OTHER_REPOSITORY: &str = "signalbox/other";
const EVENT_NAME: &str = "pull_request";
const OTHER_EVENT_NAME: &str = "check_run";
const ACTION_NAME: &str = "synchronize";
const OTHER_ACTION_NAME: &str = "completed";
const HEAD_SHA: &str = "1111111111111111111111111111111111111111";
const OTHER_HEAD_SHA: &str = "2222222222222222222222222222222222222222";
const BASE_REVISION: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const BODY: &[u8] = br#"{"repository":{"full_name":"signalbox/repository"}}"#;
const OTHER_BODY: &[u8] = br#"{"repository":{"full_name":"signalbox/other"}}"#;
const DIGEST: [u8; 32] = [0x11; 32];
const OTHER_DIGEST: [u8; 32] = [0x22; 32];
const MATCHED_IDENTITY: [u8; 32] = [0x31; 32];
const WEBHOOK_ONLY_IDENTITY: [u8; 32] = [0x32; 32];
const POLL_ONLY_IDENTITY: [u8; 32] = [0x33; 32];
const LEGACY_IDENTITY: [u8; 32] = [0x34; 32];
const HISTORICAL_IDENTITY: [u8; 32] = [0x35; 32];

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

fn other_repository() -> Result<RepositorySlug, Box<dyn Error>> {
    Ok(RepositorySlug::try_new(OTHER_REPOSITORY.to_owned())?)
}

fn pull_request(
    number: u64,
    head_sha: &str,
    head_branch: &str,
) -> Result<RepoWatchPullRequestState, Box<dyn Error>> {
    Ok(RepoWatchPullRequestState::try_new(
        RepoWatchPullRequestStateInput {
            context: PullRequestEventContext::new(PullRequestEventContextInput {
                number: PullRequestNumber::new(number.try_into()?),
                head_sha: CommitSha::try_new(head_sha.to_owned())?,
                head_repository: repository()?,
                base_branch: BranchName::try_new("main".to_owned())?,
                head_branch: BranchName::try_new(head_branch.to_owned())?,
                title: PullRequestTitle::try_new(format!("Pull request {number}"))?,
                body: PullRequestBody::try_new("Fixture body".to_owned())?,
                labels: Vec::new(),
                draft: false,
                author: Some(RepoWatchAuthorLogin::try_new("fixture-author".to_owned())?),
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

fn merge_ready_assessment(
    number: u64,
    head_sha: &str,
) -> Result<RepoWatchConvergenceAssessment, Box<dyn Error>> {
    Ok(RepoWatchConvergenceAssessment::try_new(
        RepoWatchConvergenceAssessmentInput {
            number: PullRequestNumber::new(number.try_into()?),
            head_sha: CommitSha::try_new(head_sha.to_owned())?,
            base_branch: BranchName::try_new("main".to_owned())?,
            base_revision: CommitSha::try_new(BASE_REVISION.to_owned())?,
            mergeable_state: MergeableState::Mergeable,
            review_decision: RepoWatchReviewDecision::None,
            unresolved_threads: Vec::new(),
            gating_check_count: 0,
            non_green_gating_checks: Vec::new(),
        },
    )?)
}

fn delivery_key(value: u128) -> RepoWatchWebhookDeliveryKey {
    RepoWatchWebhookDeliveryKey::new(
        NonZeroU64::new((value + 1) as u64).expect("fixture hook ID is positive"),
        Uuid::from_u128(value),
    )
}

fn admission(
    key: RepoWatchWebhookDeliveryKey,
    repository: RepositorySlug,
    event: &str,
    action: Option<&str>,
    digest: [u8; 32],
    body: &[u8],
) -> Result<RepoWatchWebhookAdmission, Box<dyn Error>> {
    Ok(RepoWatchWebhookAdmission::try_new(
        key,
        repository,
        event.to_owned(),
        action.map(str::to_owned),
        digest,
        body.to_vec(),
    )?)
}

fn pending_page_size() -> RepoWatchWebhookPendingPageSize {
    RepoWatchWebhookPendingPageSize::try_new(
        NonZeroU16::new(10).expect("fixture page size is positive"),
    )
    .expect("fixture page size is bounded")
}

fn committed_cursor(outcome: RepoWatchCommitOutcome) -> RepoWatchCursor {
    match outcome {
        RepoWatchCommitOutcome::Committed(cursor) => cursor,
        RepoWatchCommitOutcome::Unchanged(_)
        | RepoWatchCommitOutcome::Replayed(_)
        | RepoWatchCommitOutcome::Conflict { .. } => {
            panic!("fixture commit must advance the cursor")
        }
    }
}

fn projected_request(
    projections: Vec<RepoWatchWebhookProjection>,
) -> Result<RepoWatchWebhookTerminalRequest, Box<dyn Error>> {
    Ok(RepoWatchWebhookTerminalRequest::try_new(
        projections,
        RepoWatchWebhookDisposition::Projected,
        None,
    )?)
}

fn event_projection(identity: [u8; 32]) -> Result<RepoWatchWebhookProjection, Box<dyn Error>> {
    Ok(RepoWatchWebhookProjection::event(
        RepoWatchEventContentIdentityV1::from_bytes(identity),
        RepoWatchEventKindNameV1::BranchWorkflowRunCompleted,
        vec![0x41],
    )?)
}

async fn admit_fixture(
    store: &PostgresRepoWatchWebhookStore,
    key: RepoWatchWebhookDeliveryKey,
) -> Result<RepoWatchWebhookAdmissionOutcome, Box<dyn Error>> {
    Ok(store
        .admit(&admission(
            key,
            repository()?,
            EVENT_NAME,
            Some(ACTION_NAME),
            DIGEST,
            BODY,
        )?)
        .await?)
}

fn admitted_receipt(
    outcome: RepoWatchWebhookAdmissionOutcome,
) -> signalbox_persistence::repo_watch_webhook::RepoWatchWebhookReceipt {
    match outcome {
        RepoWatchWebhookAdmissionOutcome::Admitted(receipt) => receipt,
        _ => panic!("fixture admission must be new"),
    }
}

async fn seed_poll_parity_events(pool: &PgPool) -> Result<(), Box<dyn Error>> {
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO repo_watch_cursor (
            repository, generation, storage_version, cursor_payload
         ) VALUES ($1, 1, 2, $2)",
    )
    .bind(REPOSITORY)
    .bind(sqlx::types::Json(serde_json::json!({
        "storage_version": 2,
        "signal_reviewers": [],
        "event_identity_frontier": [],
        "state": {
            "pull_requests": [],
            "workflow_runs": [],
            "branch_heads": []
        }
    })))
    .execute(&mut *transaction)
    .await?;
    insert_poll_event(
        &mut transaction,
        Uuid::from_u128(0x901),
        1,
        1,
        MATCHED_IDENTITY,
        None,
    )
    .await?;
    insert_poll_event(
        &mut transaction,
        Uuid::from_u128(0x902),
        2,
        1,
        POLL_ONLY_IDENTITY,
        None,
    )
    .await?;
    insert_poll_event(
        &mut transaction,
        Uuid::from_u128(0x903),
        3,
        0,
        LEGACY_IDENTITY,
        None,
    )
    .await?;
    insert_poll_event(
        &mut transaction,
        Uuid::from_u128(0x904),
        4,
        1,
        HISTORICAL_IDENTITY,
        Some(OffsetDateTime::UNIX_EPOCH),
    )
    .await?;
    transaction.commit().await?;
    Ok(())
}

async fn insert_poll_event(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    event_id: Uuid,
    ordinal: i32,
    content_identity_version: i16,
    identity: [u8; 32],
    recorded_at: Option<OffsetDateTime>,
) -> Result<(), Box<dyn Error>> {
    sqlx::query(
        "INSERT INTO repo_watch_event (
            event_id, repository, cursor_generation, event_ordinal,
            event_version, content_identity_version, content_identity,
            producer, target_kind, event_kind, conclusion,
            workflow_branch, workflow_name, recorded_at
         ) VALUES (
            $1, $2, 1, $3, 1, $4, $5, 'poll', 'branch',
            'branch_workflow_run_completed', 'success', 'main', 'checks',
            COALESCE($6, transaction_timestamp())
         )",
    )
    .bind(event_id)
    .bind(REPOSITORY)
    .bind(ordinal)
    .bind(content_identity_version)
    .bind(identity.as_slice())
    .bind(recorded_at)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

fn expected_parity_counts() -> Vec<(String, i64)> {
    vec![
        ("matched".to_owned(), 1),
        ("not_directly_mapped".to_owned(), 3),
        ("poll_only".to_owned(), 1),
        ("webhook_only".to_owned(), 1),
    ]
}

fn expected_refresh_targets() -> Vec<(String, String)> {
    vec![
        ("pull_request_hydration".to_owned(), "40".to_owned()),
        ("mergeability".to_owned(), "41".to_owned()),
        ("check_rollup".to_owned(), HEAD_SHA.to_owned()),
    ]
}

async fn install_second_projection_rejection(pool: &PgPool) -> Result<(), Box<dyn Error>> {
    sqlx::query(
        "CREATE FUNCTION reject_second_webhook_projection()
         RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
             RAISE EXCEPTION 'fixture rejects second projection';
         END;
         $$",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE TRIGGER reject_second_webhook_projection
         BEFORE INSERT ON repo_watch_webhook_projection
         FOR EACH ROW
         WHEN (NEW.projection_ordinal = 2)
         EXECUTE FUNCTION reject_second_webhook_projection()",
    )
    .execute(pool)
    .await?;
    Ok(())
}

async fn remove_second_projection_rejection(pool: &PgPool) -> Result<(), Box<dyn Error>> {
    sqlx::query("DROP TRIGGER reject_second_webhook_projection ON repo_watch_webhook_projection")
        .execute(pool)
        .await?;
    sqlx::query("DROP FUNCTION reject_second_webhook_projection()")
        .execute(pool)
        .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn webhook_admission_distinguishes_equal_replay_from_every_conflict_field()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = PostgresRepoWatchWebhookStore::new(pool.clone());
    let equal_key = delivery_key(0x101);
    let repository_key = delivery_key(0x102);
    let event_key = delivery_key(0x103);
    let action_key = delivery_key(0x104);
    let digest_key = delivery_key(0x105);

    let first = admit_fixture(&store, equal_key).await?;
    let equal = admit_fixture(&store, equal_key).await?;
    assert_eq!(
        equal,
        RepoWatchWebhookAdmissionOutcome::EqualDuplicate(admitted_receipt(first))
    );

    admit_fixture(&store, repository_key).await?;
    let repository_conflict = store
        .admit(&admission(
            repository_key,
            other_repository()?,
            EVENT_NAME,
            Some(ACTION_NAME),
            DIGEST,
            BODY,
        )?)
        .await?;
    assert_eq!(
        repository_conflict,
        RepoWatchWebhookAdmissionOutcome::Conflict
    );

    admit_fixture(&store, event_key).await?;
    let event_conflict = store
        .admit(&admission(
            event_key,
            repository()?,
            OTHER_EVENT_NAME,
            Some(ACTION_NAME),
            DIGEST,
            BODY,
        )?)
        .await?;
    assert_eq!(event_conflict, RepoWatchWebhookAdmissionOutcome::Conflict);

    admit_fixture(&store, action_key).await?;
    let action_conflict = store
        .admit(&admission(
            action_key,
            repository()?,
            EVENT_NAME,
            Some(OTHER_ACTION_NAME),
            DIGEST,
            BODY,
        )?)
        .await?;
    assert_eq!(action_conflict, RepoWatchWebhookAdmissionOutcome::Conflict);

    admit_fixture(&store, digest_key).await?;
    let digest_conflict = store
        .admit(&admission(
            digest_key,
            repository()?,
            EVENT_NAME,
            Some(ACTION_NAME),
            OTHER_DIGEST,
            OTHER_BODY,
        )?)
        .await?;
    assert_eq!(digest_conflict, RepoWatchWebhookAdmissionOutcome::Conflict);

    let delivery_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repo_watch_webhook_delivery")
            .fetch_one(&pool)
            .await?;
    let payload_count: i64 = sqlx::query_scalar("SELECT count(*) FROM repo_watch_webhook_payload")
        .fetch_one(&pool)
        .await?;
    assert_eq!(delivery_count, 5);
    assert_eq!(payload_count, delivery_count);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn pending_delivery_survives_store_restart() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let first_store = PostgresRepoWatchWebhookStore::new(pool.clone());
    let key = delivery_key(0x201);
    admit_fixture(&first_store, key).await?;
    drop(first_store);

    let restarted_store = PostgresRepoWatchWebhookStore::new(pool);
    let pending = restarted_store
        .load_pending(&repository()?, pending_page_size())
        .await?;
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].key(), key);
    assert_eq!(pending[0].repository(), &repository()?);
    assert_eq!(pending[0].event_name(), EVENT_NAME);
    assert_eq!(pending[0].action_name(), Some(ACTION_NAME));
    assert_eq!(pending[0].body_digest(), &DIGEST);
    assert_eq!(pending[0].body(), BODY);

    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn terminal_projection_and_disposition_are_atomic() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = PostgresRepoWatchWebhookStore::new(pool.clone());
    let key = delivery_key(0x301);
    admit_fixture(&store, key).await?;
    install_second_projection_rejection(&pool).await?;
    let failed = store
        .record_terminal(
            key,
            &projected_request(vec![
                event_projection(MATCHED_IDENTITY)?,
                event_projection(WEBHOOK_ONLY_IDENTITY)?,
            ])?,
        )
        .await;
    assert!(failed.is_err());
    let projection_count_after_failure: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repo_watch_webhook_projection")
            .fetch_one(&pool)
            .await?;
    let disposition_count_after_failure: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repo_watch_webhook_disposition")
            .fetch_one(&pool)
            .await?;
    assert_eq!(projection_count_after_failure, 0);
    assert_eq!(disposition_count_after_failure, 0);
    remove_second_projection_rejection(&pool).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn terminal_disposition_drains_pending_delivery() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = PostgresRepoWatchWebhookStore::new(pool.clone());
    let key = delivery_key(0x302);
    admit_fixture(&store, key).await?;
    let recorded = store
        .record_terminal(
            key,
            &projected_request(vec![event_projection(MATCHED_IDENTITY)?])?,
        )
        .await?;
    assert_eq!(recorded, RepoWatchWebhookTerminalOutcome::Recorded);
    let repeated = store
        .record_terminal(key, &projected_request(Vec::new())?)
        .await?;
    assert_eq!(repeated, RepoWatchWebhookTerminalOutcome::AlreadyTerminal);
    let drained = store
        .load_pending(&repository()?, pending_page_size())
        .await?;
    assert!(drained.is_empty());
    let projection_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repo_watch_webhook_projection")
            .fetch_one(&pool)
            .await?;
    let disposition_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repo_watch_webhook_disposition")
            .fetch_one(&pool)
            .await?;
    assert_eq!(projection_count, 1);
    assert_eq!(disposition_count, 1);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn primary_commit_atomically_records_webhook_event_and_terminal_delivery()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let webhook_store = PostgresRepoWatchWebhookStore::new(pool.clone());
    let event_store = PostgresRepoWatchStore::new(pool.clone());
    let key = delivery_key(0x303);
    admit_fixture(&webhook_store, key).await?;
    let baseline = RepoWatchCursorCandidate::new(RepoWatchObservation::new(
        Vec::new(),
        RepoWatchRepositoryState::default(),
    ));
    let RepoWatchCommitOutcome::Committed(cursor) = event_store
        .commit(
            &repository()?,
            RepoWatchCommitRequest::new(None, baseline, Vec::new()),
        )
        .await?
    else {
        panic!("fixture baseline must commit")
    };
    let branch = BranchName::try_new("main".to_owned())?;
    let head = CommitSha::try_new(HEAD_SHA.to_owned())?;
    let observation = RepoWatchObservation::new(
        Vec::new(),
        RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput {
            pull_requests: Vec::new(),
            workflow_runs: Vec::new(),
            branch_heads: vec![RepoWatchBranchHead::new(branch.clone(), head)],
        })?,
    );
    let event = RepoWatchEvent::branch_workflow(
        RepoWatchEventId::from_uuid(Uuid::from_u128(0x303)),
        repository()?,
        branch,
        WorkflowName::try_new("checks".to_owned())?,
        CheckConclusion::Success,
    );
    let occurrence = RepoWatchEventOccurrenceV1::from_parts(
        event,
        RepoWatchEventContentIdentityV1::from_bytes(WEBHOOK_ONLY_IDENTITY),
    );
    let outcome = event_store
        .commit_webhook(
            &repository()?,
            RepoWatchCommitRequest::from_webhook(
                cursor.generation(),
                RepoWatchCursorCandidate::new(observation),
                vec![occurrence],
            ),
            key,
            vec![event_projection(WEBHOOK_ONLY_IDENTITY)?],
        )
        .await?;
    let RepoWatchCommitOutcome::Committed(committed) = outcome else {
        panic!("webhook state and event must commit")
    };

    let producer: String =
        sqlx::query_scalar("SELECT producer FROM repo_watch_event WHERE event_id = $1")
            .bind(Uuid::from_u128(0x303))
            .fetch_one(&pool)
            .await?;
    let disposition = sqlx::query_as::<_, (String, i64)>(
        "SELECT disposition, resulting_cursor_generation
           FROM repo_watch_webhook_disposition
          WHERE hook_id = $1 AND delivery_id = $2",
    )
    .bind(Decimal::from(key.hook_id().get()))
    .bind(key.delivery_id())
    .fetch_one(&pool)
    .await?;
    let pending = webhook_store
        .load_pending(&repository()?, pending_page_size())
        .await?;

    assert_eq!(producer, "webhook");
    assert_eq!(
        disposition,
        ("committed".to_owned(), committed.generation().get() as i64)
    );
    assert!(pending.is_empty());
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn primary_targeted_commit_atomically_records_partial_convergence_evidence()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let webhook_store = PostgresRepoWatchWebhookStore::new(pool.clone());
    let event_store = PostgresRepoWatchStore::new(pool.clone());
    let key = delivery_key(0x305);
    admit_fixture(&webhook_store, key).await?;
    let baseline = RepoWatchCursorCandidate::new(RepoWatchObservation::new(
        Vec::new(),
        RepoWatchRepositoryState::default(),
    ));
    let RepoWatchCommitOutcome::Committed(cursor) = event_store
        .commit(
            &repository()?,
            RepoWatchCommitRequest::new(None, baseline, Vec::new()),
        )
        .await?
    else {
        panic!("fixture baseline must commit")
    };
    let observation = RepoWatchObservation::new(
        Vec::new(),
        RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput {
            pull_requests: vec![
                pull_request(41, HEAD_SHA, "agent/targeted")?,
                pull_request(42, OTHER_HEAD_SHA, "agent/untouched")?,
            ],
            workflow_runs: Vec::new(),
            branch_heads: vec![RepoWatchBranchHead::new(
                BranchName::try_new("main".to_owned())?,
                CommitSha::try_new(BASE_REVISION.to_owned())?,
            )],
        })?,
    );

    let outcome = event_store
        .commit_webhook_with_convergence(
            &repository()?,
            RepoWatchCommitRequest::from_webhook(
                cursor.generation(),
                RepoWatchCursorCandidate::new(observation),
                Vec::new(),
            ),
            key,
            Vec::new(),
            &[merge_ready_assessment(41, HEAD_SHA)?],
        )
        .await?;
    let RepoWatchCommitOutcome::Committed(committed) = outcome else {
        panic!("targeted webhook state and convergence evidence must commit")
    };
    let assessments = sqlx::query_as::<_, (String, Decimal)>(
        "SELECT head_sha, pull_request_number
           FROM repo_watch_pull_request_convergence_assessment
          ORDER BY pull_request_number",
    )
    .fetch_all(&pool)
    .await?;
    let seal_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repo_watch_pull_request_convergence")
            .fetch_one(&pool)
            .await?;
    let disposition_generation: i64 = sqlx::query_scalar(
        "SELECT resulting_cursor_generation
           FROM repo_watch_webhook_disposition
          WHERE hook_id = $1 AND delivery_id = $2",
    )
    .bind(Decimal::from(key.hook_id().get()))
    .bind(key.delivery_id())
    .fetch_one(&pool)
    .await?;

    assert_eq!(
        assessments,
        vec![(HEAD_SHA.to_owned(), Decimal::from(41_u64))]
    );
    assert_eq!(seal_count, 1);
    assert_eq!(disposition_generation, committed.generation().get() as i64);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn primary_targeted_commit_records_evidence_before_its_base_branch()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let webhook_store = PostgresRepoWatchWebhookStore::new(pool.clone());
    let event_store = PostgresRepoWatchStore::new(pool.clone());
    let key = delivery_key(0x306);
    admit_fixture(&webhook_store, key).await?;
    let baseline = RepoWatchCursorCandidate::new(RepoWatchObservation::new(
        Vec::new(),
        RepoWatchRepositoryState::default(),
    ));
    let cursor = committed_cursor(
        event_store
            .commit(
                &repository()?,
                RepoWatchCommitRequest::new(None, baseline, Vec::new()),
            )
            .await?,
    );
    let observation = RepoWatchObservation::new(
        Vec::new(),
        RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput {
            pull_requests: vec![pull_request(41, HEAD_SHA, "agent/targeted")?],
            workflow_runs: Vec::new(),
            branch_heads: Vec::new(),
        })?,
    );

    let committed = committed_cursor(
        event_store
            .commit_webhook_with_convergence(
                &repository()?,
                RepoWatchCommitRequest::from_webhook(
                    cursor.generation(),
                    RepoWatchCursorCandidate::new(observation),
                    Vec::new(),
                ),
                key,
                Vec::new(),
                &[merge_ready_assessment(41, HEAD_SHA)?],
            )
            .await?,
    );
    let assessments = sqlx::query_as::<_, (String, String)>(
        "SELECT head_sha, base_branch
           FROM repo_watch_pull_request_convergence_assessment",
    )
    .fetch_all(&pool)
    .await?;
    let disposition_generation: i64 = sqlx::query_scalar(
        "SELECT resulting_cursor_generation
           FROM repo_watch_webhook_disposition
          WHERE hook_id = $1 AND delivery_id = $2",
    )
    .bind(Decimal::from(key.hook_id().get()))
    .bind(key.delivery_id())
    .fetch_one(&pool)
    .await?;

    assert_eq!(assessments, vec![(HEAD_SHA.to_owned(), "main".to_owned())]);
    assert_eq!(disposition_generation, committed.generation().get() as i64);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn primary_commit_rolls_back_cursor_and_event_when_terminal_delivery_is_missing()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let event_store = PostgresRepoWatchStore::new(pool.clone());
    let baseline = RepoWatchCursorCandidate::new(RepoWatchObservation::new(
        Vec::new(),
        RepoWatchRepositoryState::default(),
    ));
    let RepoWatchCommitOutcome::Committed(cursor) = event_store
        .commit(
            &repository()?,
            RepoWatchCommitRequest::new(None, baseline, Vec::new()),
        )
        .await?
    else {
        panic!("fixture baseline must commit")
    };
    let branch = BranchName::try_new("main".to_owned())?;
    let observation = RepoWatchObservation::new(
        Vec::new(),
        RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput {
            pull_requests: Vec::new(),
            workflow_runs: Vec::new(),
            branch_heads: vec![RepoWatchBranchHead::new(
                branch.clone(),
                CommitSha::try_new(HEAD_SHA.to_owned())?,
            )],
        })?,
    );
    let event_id = RepoWatchEventId::from_uuid(Uuid::from_u128(0x304));
    let event = RepoWatchEvent::branch_workflow(
        event_id,
        repository()?,
        branch,
        WorkflowName::try_new("checks".to_owned())?,
        CheckConclusion::Success,
    );
    let occurrence = RepoWatchEventOccurrenceV1::from_parts(
        event,
        RepoWatchEventContentIdentityV1::from_bytes(POLL_ONLY_IDENTITY),
    );
    let result = event_store
        .commit_webhook(
            &repository()?,
            RepoWatchCommitRequest::from_webhook(
                cursor.generation(),
                RepoWatchCursorCandidate::new(observation),
                vec![occurrence],
            ),
            delivery_key(0x304),
            vec![event_projection(POLL_ONLY_IDENTITY)?],
        )
        .await;
    let Err(RepoWatchStoreError::WebhookTerminal(RepoWatchWebhookStoreError::MissingDelivery)) =
        result
    else {
        panic!("a missing delivery must reject the atomic primary commit")
    };

    let stored_cursor = event_store
        .load_cursor(&repository()?)
        .await?
        .expect("the baseline cursor remains");
    let event_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repo_watch_event WHERE event_id = $1")
            .bind(Uuid::from_u128(0x304))
            .fetch_one(&pool)
            .await?;

    assert_eq!(stored_cursor, cursor);
    assert_eq!(event_count, 0);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn parity_view_classifies_all_four_shadow_statuses() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = PostgresRepoWatchWebhookStore::new(pool.clone());
    let matched_key = delivery_key(0x401);
    let webhook_only_key = delivery_key(0x402);
    let refresh_key = delivery_key(0x403);
    admit_fixture(&store, matched_key).await?;
    admit_fixture(&store, webhook_only_key).await?;
    admit_fixture(&store, refresh_key).await?;
    store
        .record_terminal(
            matched_key,
            &projected_request(vec![event_projection(MATCHED_IDENTITY)?])?,
        )
        .await?;
    store
        .record_terminal(
            webhook_only_key,
            &projected_request(vec![event_projection(WEBHOOK_ONLY_IDENTITY)?])?,
        )
        .await?;
    store
        .record_terminal(
            refresh_key,
            &projected_request(vec![
                RepoWatchWebhookProjection::TargetedQuery(
                    RepoWatchWebhookTargetedQuery::PullRequestHydration(PullRequestNumber::new(
                        NonZeroU64::new(40).expect("fixture PR number is positive"),
                    )),
                ),
                RepoWatchWebhookProjection::TargetedQuery(
                    RepoWatchWebhookTargetedQuery::Mergeability(PullRequestNumber::new(
                        NonZeroU64::new(41).expect("fixture PR number is positive"),
                    )),
                ),
                RepoWatchWebhookProjection::TargetedQuery(
                    RepoWatchWebhookTargetedQuery::CheckRollup(CommitSha::try_new(
                        HEAD_SHA.to_owned(),
                    )?),
                ),
            ])?,
        )
        .await?;
    seed_poll_parity_events(&pool).await?;

    let statuses = sqlx::query_as::<_, (String, i64)>(
        "SELECT status, count(*)
           FROM repo_watch_webhook_parity
          GROUP BY status
          ORDER BY status",
    )
    .fetch_all(&pool)
    .await?;
    assert_eq!(statuses, expected_parity_counts());
    let refresh = sqlx::query_as::<_, (String, String)>(
        "SELECT targeted_query_kind, targeted_query_key
           FROM repo_watch_webhook_parity
          WHERE status = 'not_directly_mapped'
          ORDER BY projection_ordinal",
    )
    .fetch_all(&pool)
    .await?;
    assert_eq!(refresh, expected_refresh_targets());
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn payload_retention_requires_terminal_state_and_seven_days() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = PostgresRepoWatchWebhookStore::new(pool.clone());
    let fresh_key = delivery_key(0x501);
    admit_fixture(&store, fresh_key).await?;
    store
        .record_terminal(fresh_key, &projected_request(Vec::new())?)
        .await?;
    seed_old_delivery(&pool, delivery_key(0x502), true).await?;
    seed_old_delivery(&pool, delivery_key(0x503), false).await?;
    seed_old_delivery_with_recent_terminal(&pool, delivery_key(0x504)).await?;

    let purged = store.purge_expired_payloads().await?;
    assert_eq!(purged, 1);
    let payload_count: i64 = sqlx::query_scalar("SELECT count(*) FROM repo_watch_webhook_payload")
        .fetch_one(&pool)
        .await?;
    let tombstone_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repo_watch_webhook_delivery")
            .fetch_one(&pool)
            .await?;
    assert_eq!(payload_count, 3);
    assert_eq!(tombstone_count, 4);
    let premature_delete = sqlx::query(
        "DELETE FROM repo_watch_webhook_payload
          WHERE hook_id = $1 AND delivery_id = $2",
    )
    .bind(Decimal::from(fresh_key.hook_id().get()))
    .bind(fresh_key.delivery_id())
    .execute(&pool)
    .await;
    assert!(premature_delete.is_err());
    Ok(())
}

async fn seed_old_delivery(
    pool: &PgPool,
    key: RepoWatchWebhookDeliveryKey,
    terminal: bool,
) -> Result<(), Box<dyn Error>> {
    let mut transaction = pool.begin().await?;
    sqlx::query(
        "INSERT INTO repo_watch_webhook_delivery (
            hook_id, delivery_id, repository, event_name, action_name,
            body_digest, received_at
         ) VALUES ($1, $2, $3, $4, $5, $6,
                   statement_timestamp() - interval '8 days')",
    )
    .bind(Decimal::from(key.hook_id().get()))
    .bind(key.delivery_id())
    .bind(REPOSITORY)
    .bind(EVENT_NAME)
    .bind(ACTION_NAME)
    .bind(DIGEST.as_slice())
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO repo_watch_webhook_payload (hook_id, delivery_id, body)
         VALUES ($1, $2, $3)",
    )
    .bind(Decimal::from(key.hook_id().get()))
    .bind(key.delivery_id())
    .bind(BODY)
    .execute(&mut *transaction)
    .await?;
    if terminal {
        sqlx::query(
            "INSERT INTO repo_watch_webhook_disposition (
                hook_id, delivery_id, disposition, recorded_at
             ) VALUES (
                $1, $2, 'ignored', statement_timestamp() - interval '8 days'
             )",
        )
        .bind(Decimal::from(key.hook_id().get()))
        .bind(key.delivery_id())
        .execute(&mut *transaction)
        .await?;
    }
    transaction.commit().await?;
    Ok(())
}

async fn seed_old_delivery_with_recent_terminal(
    pool: &PgPool,
    key: RepoWatchWebhookDeliveryKey,
) -> Result<(), Box<dyn Error>> {
    seed_old_delivery(pool, key, false).await?;
    sqlx::query(
        "INSERT INTO repo_watch_webhook_disposition (
            hook_id, delivery_id, disposition
         ) VALUES ($1, $2, 'ignored')",
    )
    .bind(Decimal::from(key.hook_id().get()))
    .bind(key.delivery_id())
    .execute(pool)
    .await?;
    Ok(())
}
