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
use signalbox_application::RepoWatchEventContentIdentityV1;
use signalbox_application::{
    RepoWatchObservation, RepoWatchRepositoryState, RepoWatchRepositoryStateInput,
};
use signalbox_domain::{CommitSha, PullRequestNumber, RepoWatchEventKindNameV1, RepositorySlug};
use signalbox_persistence::{
    disposable_postgres_server_args, disposable_postgres_state_tmpfs,
    disposable_test_container_labels, local_test_connection_options, migrate,
    repo_watch::{
        PostgresRepoWatchStore, RepoWatchCommitOutcome, RepoWatchCommitRequest,
        RepoWatchCursorCandidate, RepoWatchCursorGeneration,
    },
    repo_watch_webhook::{
        MAX_PENDING_PAGE_BYTES, PostgresRepoWatchWebhookStore, RepoWatchWebhookAdmission,
        RepoWatchWebhookAdmissionOutcome, RepoWatchWebhookDeliveryKey, RepoWatchWebhookDisposition,
        RepoWatchWebhookParityCauseV1, RepoWatchWebhookPendingPageSize, RepoWatchWebhookProjection,
        RepoWatchWebhookTargetedQuery, RepoWatchWebhookTerminalOutcome,
        RepoWatchWebhookTerminalRequest,
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
const BODY: &[u8] = br#"{"repository":{"full_name":"signalbox/repository"}}"#;
const OTHER_BODY: &[u8] = br#"{"repository":{"full_name":"signalbox/other"}}"#;
const DIGEST: [u8; 32] = [0x11; 32];
const OTHER_DIGEST: [u8; 32] = [0x22; 32];
const LARGE_BODY_BYTES: usize = 4 * 1024 * 1024;
const LARGE_DELIVERY_BASE: u128 = 0x900;
const MATCHED_IDENTITY: [u8; 32] = [0x31; 32];
const WEBHOOK_ONLY_IDENTITY: [u8; 32] = [0x32; 32];
const POLL_ONLY_IDENTITY: [u8; 32] = [0x33; 32];
const HISTORICAL_IDENTITY: [u8; 32] = [0x35; 32];

async fn migrated_postgres() -> Result<(ContainerAsync<Postgres>, PgPool), Box<dyn Error>> {
    let container = Postgres::default()
        .with_db_name(DATABASE_NAME)
        .with_user(DATABASE_USER)
        .with_password(DATABASE_PASSWORD)
        .with_cmd(disposable_postgres_server_args())
        .with_mount(disposable_postgres_state_tmpfs())
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

/// Admits `count` deliveries whose bodies are each `body_bytes` long.
///
/// The loop lives here rather than in a test body so each test stays
/// straight-line, as `docs/agents/testing-style.md` rule 2 requires.
async fn seed_sized_pending_deliveries(
    store: &PostgresRepoWatchWebhookStore,
    count: usize,
    body_bytes: usize,
) -> Result<(), Box<dyn Error>> {
    for index in 0..count {
        let body = vec![b'x'; body_bytes];
        store
            .admit(&admission(
                delivery_key(LARGE_DELIVERY_BASE + index as u128),
                repository()?,
                EVENT_NAME,
                Some(ACTION_NAME),
                DIGEST,
                &body,
            )?)
            .await?;
    }
    Ok(())
}

fn pending_page_size() -> RepoWatchWebhookPendingPageSize {
    RepoWatchWebhookPendingPageSize::try_new(
        NonZeroU16::new(10).expect("fixture page size is positive"),
    )
    .expect("fixture page size is bounded")
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
        None,
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
        MATCHED_IDENTITY,
        None,
    )
    .await?;
    insert_poll_event(
        &mut transaction,
        Uuid::from_u128(0x902),
        2,
        POLL_ONLY_IDENTITY,
        None,
    )
    .await?;
    insert_poll_event(
        &mut transaction,
        Uuid::from_u128(0x904),
        4,
        HISTORICAL_IDENTITY,
        Some(OffsetDateTime::UNIX_EPOCH),
    )
    .await?;
    transaction.commit().await?;
    Ok(())
}

/// Seeds one poll-produced event. Exactly one content-identity version is
/// storable, so the version the parity view filters on is not a fixture axis.
/// Seeds one poll event of a family webhooks are not designed to reproduce.
async fn seed_poll_only_family_event(pool: &PgPool) -> Result<(), Box<dyn Error>> {
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
    sqlx::query(
        "INSERT INTO repo_watch_event (
            event_id, repository, cursor_generation, event_ordinal,
            event_version, content_identity_version, content_identity,
            producer, target_kind, event_kind, checks_outcome,
            pull_request_number, head_sha, head_repository, base_branch,
            head_branch, title, body, labels, draft, recorded_at
         ) VALUES (
            $1, $2, 1, 1, 1, 1, $3, 'poll', 'pull_request',
            'checks_completed', 'success',
            7, $4, $2, 'main', 'topic', 'fixture', '', ARRAY[]::text[], false,
            transaction_timestamp()
         )",
    )
    .bind(Uuid::from_u128(0x9F1))
    .bind(REPOSITORY)
    .bind(POLL_ONLY_IDENTITY.as_slice())
    .bind(HEAD_SHA)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(())
}

async fn insert_poll_event(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    event_id: Uuid,
    ordinal: i32,
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
            $1, $2, 1, $3, 1, 1, $4, 'poll', 'branch',
            'branch_workflow_run_completed', 'success', 'main', 'checks',
            COALESCE($5, transaction_timestamp())
         )",
    )
    .bind(event_id)
    .bind(REPOSITORY)
    .bind(ordinal)
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
        .load_pending(&repository()?, pending_page_size(), None)
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

/// The drain monitor reads the oldest pending delivery on a fixed cadence for
/// every webhook repository, so it must not transfer the admitted bodies that a
/// pending page carries. Taking the payload table out of reach proves the
/// dependency rather than asserting it: the monitor query still answers where
/// the page query, which joins that table, can no longer run at all.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn oldest_pending_receipt_is_read_without_its_payload() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = PostgresRepoWatchWebhookStore::new(pool.clone());
    let oldest = delivery_key(0x401);
    let newer = delivery_key(0x402);
    admit_fixture(&store, oldest).await?;
    admit_fixture(&store, newer).await?;
    store
        .admit(&admission(
            delivery_key(0x403),
            other_repository()?,
            OTHER_EVENT_NAME,
            Some(OTHER_ACTION_NAME),
            OTHER_DIGEST,
            OTHER_BODY,
        )?)
        .await?;

    let pending = store
        .load_oldest_pending_receipt(&repository()?)
        .await?
        .expect("an admitted delivery is pending");

    assert_eq!(pending.key(), oldest);
    let page = store
        .load_pending(&repository()?, pending_page_size(), None)
        .await?;
    assert_eq!(pending.receipt(), page[0].receipt());

    sqlx::query(
        "ALTER TABLE repo_watch_webhook_payload RENAME TO repo_watch_webhook_payload_hidden",
    )
    .execute(&pool)
    .await?;

    let without_payload = store
        .load_oldest_pending_receipt(&repository()?)
        .await?
        .expect("the monitor query does not depend on the payload");
    assert_eq!(without_payload.key(), oldest);
    assert_eq!(without_payload.receipt(), pending.receipt());
    assert!(
        store
            .load_pending(&repository()?, pending_page_size(), None)
            .await
            .is_err(),
        "the drain page joins the payload the monitor deliberately never reads"
    );

    sqlx::query(
        "ALTER TABLE repo_watch_webhook_payload_hidden RENAME TO repo_watch_webhook_payload",
    )
    .execute(&pool)
    .await?;

    Ok(())
}

/// The transactional pending inventory, rather than disposition-history scans,
/// advances the monitor and page reads as deliveries reach terminal state.
#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn the_oldest_pending_receipt_advances_as_deliveries_reach_terminal_state()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = PostgresRepoWatchWebhookStore::new(pool.clone());
    let oldest = delivery_key(0x411);
    let newer = delivery_key(0x412);
    admit_fixture(&store, oldest).await?;
    admit_fixture(&store, newer).await?;
    assert_eq!(
        store
            .load_oldest_pending_receipt(&repository()?)
            .await?
            .map(|pending| pending.key()),
        Some(oldest)
    );

    store
        .record_terminal(oldest, &projected_request(Vec::new())?)
        .await?;

    sqlx::query(
        "ALTER TABLE repo_watch_webhook_disposition
         RENAME TO repo_watch_webhook_disposition_hidden",
    )
    .execute(&pool)
    .await?;
    assert_eq!(
        store
            .load_oldest_pending_receipt(&repository()?)
            .await?
            .map(|pending| pending.key()),
        Some(newer)
    );
    assert_eq!(
        store
            .load_pending(&repository()?, pending_page_size(), None)
            .await?[0]
            .key(),
        newer
    );
    sqlx::query(
        "ALTER TABLE repo_watch_webhook_disposition_hidden
         RENAME TO repo_watch_webhook_disposition",
    )
    .execute(&pool)
    .await?;

    store
        .record_terminal(newer, &projected_request(Vec::new())?)
        .await?;

    assert_eq!(
        store.load_oldest_pending_receipt(&repository()?).await?,
        None
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn pending_inventory_changes_only_with_admission_and_disposition()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = PostgresRepoWatchWebhookStore::new(pool.clone());
    let key = delivery_key(0x413);
    admit_fixture(&store, key).await?;

    let pending_count: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM repo_watch_webhook_pending
          WHERE hook_id = $1 AND delivery_id = $2",
    )
    .bind(Decimal::from(key.hook_id().get()))
    .bind(key.delivery_id())
    .fetch_one(&pool)
    .await?;
    assert_eq!(pending_count, 1);

    let update_error = sqlx::query(
        "UPDATE repo_watch_webhook_pending
            SET repository = $1
          WHERE hook_id = $2 AND delivery_id = $3",
    )
    .bind(OTHER_REPOSITORY)
    .bind(Decimal::from(key.hook_id().get()))
    .bind(key.delivery_id())
    .execute(&pool)
    .await
    .expect_err("pending inventory rejects updates");
    assert!(update_error.to_string().contains("cannot be updated"));

    let delete_error = sqlx::query(
        "DELETE FROM repo_watch_webhook_pending
          WHERE hook_id = $1 AND delivery_id = $2",
    )
    .bind(Decimal::from(key.hook_id().get()))
    .bind(key.delivery_id())
    .execute(&pool)
    .await
    .expect_err("pending inventory rejects deletion before disposition");
    assert!(
        delete_error
            .to_string()
            .contains("retires only with its disposition")
    );

    store
        .record_terminal(key, &projected_request(Vec::new())?)
        .await?;
    let terminal_pending_count: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM repo_watch_webhook_pending
          WHERE hook_id = $1 AND delivery_id = $2",
    )
    .bind(Decimal::from(key.hook_id().get()))
    .bind(key.delivery_id())
    .fetch_one(&pool)
    .await?;
    assert_eq!(terminal_pending_count, 0);
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
        .load_pending(&repository()?, pending_page_size(), None)
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
async fn pending_page_stops_at_the_retained_byte_ceiling() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = PostgresRepoWatchWebhookStore::new(pool);
    let admitted = MAX_PENDING_PAGE_BYTES / LARGE_BODY_BYTES + 1;
    seed_sized_pending_deliveries(&store, admitted, LARGE_BODY_BYTES).await?;

    let page = store
        .load_pending(&repository()?, pending_page_size(), None)
        .await?;

    assert_eq!(page.len(), MAX_PENDING_PAGE_BYTES / LARGE_BODY_BYTES);
    assert!(page.len() < admitted);
    assert_eq!(page[0].key(), delivery_key(LARGE_DELIVERY_BASE));
    assert_eq!(page[0].body().len(), LARGE_BODY_BYTES);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn one_body_above_the_page_ceiling_still_drains() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = PostgresRepoWatchWebhookStore::new(pool);
    seed_sized_pending_deliveries(&store, 1, MAX_PENDING_PAGE_BYTES + 1).await?;

    let page = store
        .load_pending(&repository()?, pending_page_size(), None)
        .await?;

    assert_eq!(page.len(), 1);
    assert_eq!(page[0].body().len(), MAX_PENDING_PAGE_BYTES + 1);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn committed_disposition_is_refused_by_the_schema() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = PostgresRepoWatchWebhookStore::new(pool.clone());
    let key = delivery_key(0x501);
    admit_fixture(&store, key).await?;

    let rejected = sqlx::query(
        "INSERT INTO repo_watch_webhook_disposition (hook_id, delivery_id, disposition)
         VALUES ($1, $2, 'committed')",
    )
    .bind(Decimal::from(key.hook_id().get()))
    .bind(key.delivery_id())
    .execute(&pool)
    .await;

    assert!(
        rejected.is_err(),
        "shadow mode reserves no committed disposition"
    );
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

/// One projected occurrence that already names why it may not match.
fn caused_event_projection(
    identity: [u8; 32],
    cause: RepoWatchWebhookParityCauseV1,
) -> Result<RepoWatchWebhookProjection, Box<dyn Error>> {
    Ok(RepoWatchWebhookProjection::event(
        RepoWatchEventContentIdentityV1::from_bytes(identity),
        RepoWatchEventKindNameV1::BranchWorkflowRunCompleted,
        vec![0x41],
        Some(cause),
    )?)
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_stored_projection_rejects_the_derived_poll_only_cause() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = PostgresRepoWatchWebhookStore::new(pool.clone());
    let key = delivery_key(0x701);
    admit_fixture(&store, key).await?;

    let rejected = store
        .record_terminal(
            key,
            &projected_request(vec![caused_event_projection(
                WEBHOOK_ONLY_IDENTITY,
                RepoWatchWebhookParityCauseV1::PollOnlyFamily,
            )?])?,
        )
        .await;

    assert!(
        rejected.is_err(),
        "poll_only_family is derived for poll-side parity rows and must never be stored"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_webhook_only_row_reports_the_cause_its_delivery_recorded() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    let store = PostgresRepoWatchWebhookStore::new(pool.clone());
    let key = delivery_key(0x601);
    admit_fixture(&store, key).await?;
    store
        .record_terminal(
            key,
            &projected_request(vec![caused_event_projection(
                WEBHOOK_ONLY_IDENTITY,
                RepoWatchWebhookParityCauseV1::CrossDrainShadowGap,
            )?])?,
        )
        .await?;

    let causes = sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT status, cause FROM repo_watch_webhook_parity
          WHERE projection_kind = 'event'",
    )
    .fetch_all(&pool)
    .await?;

    assert_eq!(
        causes,
        vec![(
            "webhook_only".to_owned(),
            Some("cross_drain_shadow_gap".to_owned())
        )]
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_poll_only_family_row_derives_its_cause() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = PostgresRepoWatchWebhookStore::new(pool.clone());
    let key = delivery_key(0x602);
    admit_fixture(&store, key).await?;
    store
        .record_terminal(key, &projected_request(Vec::new())?)
        .await?;
    seed_poll_only_family_event(&pool).await?;

    let unexplained = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM repo_watch_webhook_parity
          WHERE status IN ('webhook_only', 'poll_only') AND cause IS NULL",
    )
    .fetch_one(&pool)
    .await?;
    let derived = sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT status, cause FROM repo_watch_webhook_parity
          WHERE status = 'poll_only'",
    )
    .fetch_all(&pool)
    .await?;

    assert_eq!(
        derived,
        vec![("poll_only".to_owned(), Some("poll_only_family".to_owned()))]
    );
    assert_eq!(unexplained, 0);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn an_uncaused_divergence_fails_the_parity_gate() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let store = PostgresRepoWatchWebhookStore::new(pool.clone());
    let key = delivery_key(0x603);
    admit_fixture(&store, key).await?;
    store
        .record_terminal(
            key,
            &projected_request(vec![event_projection(WEBHOOK_ONLY_IDENTITY)?])?,
        )
        .await?;

    let unexplained = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM repo_watch_webhook_parity
          WHERE status IN ('webhook_only', 'poll_only') AND cause IS NULL",
    )
    .fetch_one(&pool)
    .await?;

    assert_eq!(unexplained, 1);
    Ok(())
}

/// A cursor commit that loses its generation race, writing nothing.
fn conflicting_commit_request() -> Result<RepoWatchCommitRequest, Box<dyn Error>> {
    Ok(RepoWatchCommitRequest::new(
        Some(RepoWatchCursorGeneration::INITIAL),
        RepoWatchCursorCandidate::new(RepoWatchObservation::new(
            Vec::new(),
            RepoWatchRepositoryState::try_new(RepoWatchRepositoryStateInput::default())?,
        )),
        Vec::new(),
    ))
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_failed_cursor_commit_leaves_its_delivery_retryable() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let webhook = PostgresRepoWatchWebhookStore::new(pool.clone());
    let cursors = PostgresRepoWatchStore::new(pool);
    let key = delivery_key(0x701);
    admit_fixture(&webhook, key).await?;

    let outcome = cursors
        .commit(&repository()?, conflicting_commit_request()?)
        .await?;

    assert!(matches!(outcome, RepoWatchCommitOutcome::Conflict { .. }));
    let pending = webhook
        .load_pending(&repository()?, pending_page_size(), None)
        .await?;
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].key(), key);
    Ok(())
}

#[tokio::test]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_delivery_recorded_before_its_cursor_commit_cannot_be_retried()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let webhook = PostgresRepoWatchWebhookStore::new(pool.clone());
    let cursors = PostgresRepoWatchStore::new(pool);
    let key = delivery_key(0x702);
    admit_fixture(&webhook, key).await?;
    webhook
        .record_terminal(key, &projected_request(Vec::new())?)
        .await?;

    let outcome = cursors
        .commit(&repository()?, conflicting_commit_request()?)
        .await?;

    // This is what recording a delivery terminal before its cursor commit costs:
    // the commit writes nothing and the delivery can never be loaded again.
    assert!(matches!(outcome, RepoWatchCommitOutcome::Conflict { .. }));
    let pending = webhook
        .load_pending(&repository()?, pending_page_size(), None)
        .await?;
    assert!(pending.is_empty());
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
