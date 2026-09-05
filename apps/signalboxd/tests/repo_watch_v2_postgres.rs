#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "the standalone integration test uses assertion panics and explicit fixture expectations"
)]

use std::{error::Error, num::NonZeroU64, time::Duration};

use signalbox_module_repo_watch_v2::{
    PullRequestLifecycle, PullRequestState, RepoWatchStore, RepositoryState, WebhookAdmission,
    WebhookDelivery, WebhookDisposition,
};
use signalbox_ownership_seam::{
    BranchName, CommitSha, OffsetDateTime, PullRequestBody, PullRequestNumber, PullRequestTitle,
    RepoWatchAuthorLogin, RepositorySlug,
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
