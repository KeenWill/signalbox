#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

//! Pins the durable Git remote authority schema against the domain newtypes.
//!
//! The migration's `CHECK` predicates restate, in SQL, rules the domain
//! newtypes enforce in Rust. Nothing links the two declarations, so each rule is
//! exercised here through the real constraint with the value the Rust
//! constructor accepted or refused.

use std::{error::Error, time::Duration};

use signalbox_domain::{GitRemoteName, GitRemoteUrl, GitRemoteWorkspaceRoot};
use signalbox_persistence::{local_test_connection_options, migrate};
use sqlx::{PgConnection, PgPool, postgres::PgPoolOptions, types::Uuid};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
};

const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
const DATABASE_NAME: &str = "signalbox_git_remote_authority";
const DATABASE_USER: &str = "signalbox";
const DATABASE_PASSWORD: &str = "signalbox-test-only";

const WORKSPACE: &str = "/srv/signalbox/workspace";
const NAME: &str = "origin";
const URL: &str = "https://example.test/namespace/project.git";

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

/// One durable-command identity, distinct per seed and otherwise arbitrary.
///
/// The seed carries no meaning beyond uniqueness. The three identity kinds are
/// drawn from disjoint ranges so a transposed same-typed argument names nothing
/// that exists rather than silently naming a row of the other kind.
fn command_id(seed: u128) -> Uuid {
    Uuid::from_u128(0xc0de_0000 + seed)
}

/// One mint identity, distinct per seed and otherwise arbitrary.
fn mint_id(seed: u128) -> Uuid {
    Uuid::from_u128(0xa11d_0000 + seed)
}

/// One withdrawal identity, distinct per seed and otherwise arbitrary.
fn withdrawal_id(seed: u128) -> Uuid {
    Uuid::from_u128(0xbeef_0000 + seed)
}

/// Records one durable command of the given kind.
async fn insert_command(
    connection: &mut PgConnection,
    command: Uuid,
    kind: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO durable_command (command_id, command_kind, storage_version, claimed_at)
         VALUES ($1, $2, 1, now())",
    )
    .bind(command)
    .bind(kind)
    .execute(connection)
    .await
    .map(|_| ())
}

/// Records one mint row bound to an already-recorded command.
async fn insert_mint(
    connection: &mut PgConnection,
    mint: Uuid,
    command: Uuid,
    workspace_root: &str,
    remote_name: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO configured_git_remote_mint
             (mint_id, command_id, command_kind, storage_version,
              workspace_root, remote_name, remote_url)
         VALUES ($1, $2, 'mint_git_remote', 1, $3, $4, $5)",
    )
    .bind(mint)
    .bind(command)
    .bind(workspace_root)
    .bind(remote_name)
    .bind(URL)
    .execute(connection)
    .await
    .map(|_| ())
}

/// Mints one destination in its own committed transaction.
async fn mint(
    pool: &PgPool,
    command: Uuid,
    mint: Uuid,
    workspace_root: &str,
    remote_name: &str,
) -> Result<(), sqlx::Error> {
    let mut transaction = pool.begin().await?;
    insert_command(&mut transaction, command, "mint_git_remote").await?;
    insert_mint(&mut transaction, mint, command, workspace_root, remote_name).await?;
    transaction.commit().await
}

/// Counts mints that stand un-withdrawn.
async fn live_mints(pool: &PgPool) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT count(*)
           FROM configured_git_remote_mint AS mint
          WHERE NOT EXISTS (
                SELECT 1 FROM configured_git_remote_withdrawal AS withdrawal
                 WHERE withdrawal.mint_id = mint.mint_id
              )",
    )
    .fetch_one(pool)
    .await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_minted_remote_commits_through_the_durable_command_registry() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;

    mint(&pool, command_id(1), mint_id(1), WORKSPACE, NAME).await?;

    assert_eq!(
        live_mints(&pool).await?,
        1,
        "the mint kind reaches the typed-record registry"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_later_mint_for_one_workspace_and_name_is_refused() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    mint(&pool, command_id(1), mint_id(1), WORKSPACE, NAME).await?;

    let refused = mint(&pool, command_id(2), mint_id(2), WORKSPACE, NAME).await;

    assert!(
        refused.is_err(),
        "one workspace and name resolve to at most one live destination"
    );
    assert_eq!(live_mints(&pool).await?, 1);
    Ok(())
}

/// The deferred count trigger cannot see another transaction's uncommitted row,
/// so `configured_git_remote_mint_serializes_name` takes an advisory lock held
/// to end of transaction. This test fails if that trigger is removed: without
/// it the contender's insert returns immediately, because the count is not
/// checked until commit.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_simultaneous_mint_for_one_name_waits_for_the_holder() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;

    let mut holder = pool.begin().await?;
    insert_command(&mut holder, command_id(1), "mint_git_remote").await?;
    insert_mint(&mut holder, mint_id(1), command_id(1), WORKSPACE, NAME).await?;

    let mut contender = pool.begin().await?;
    insert_command(&mut contender, command_id(2), "mint_git_remote").await?;
    let contended = tokio::time::timeout(
        Duration::from_secs(3),
        insert_mint(&mut contender, mint_id(2), command_id(2), WORKSPACE, NAME),
    )
    .await;

    assert!(
        contended.is_err(),
        "the contending mint inserted while an uncommitted mint held the same name"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_withdrawal_and_its_replacement_land_in_one_transaction() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    mint(&pool, command_id(1), mint_id(1), WORKSPACE, NAME).await?;

    let mut transaction = pool.begin().await?;
    insert_command(&mut transaction, command_id(2), "withdraw_git_remote").await?;
    sqlx::query(
        "INSERT INTO configured_git_remote_withdrawal
             (withdrawal_id, mint_id, command_id, command_kind, storage_version)
         VALUES ($1, $2, $3, 'withdraw_git_remote', 1)",
    )
    .bind(withdrawal_id(1))
    .bind(mint_id(1))
    .bind(command_id(2))
    .execute(&mut *transaction)
    .await?;
    insert_command(&mut transaction, command_id(3), "mint_git_remote").await?;
    insert_mint(&mut transaction, mint_id(2), command_id(3), WORKSPACE, NAME).await?;
    transaction.commit().await?;

    assert_eq!(
        live_mints(&pool).await?,
        1,
        "the replacement is the sole live destination"
    );
    Ok(())
}

/// The row-local `CHECK` pins a mint row's own discriminator.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_mint_row_stating_another_command_kind_is_refused() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;

    let mut transaction = pool.begin().await?;
    let refused = sqlx::query(
        "INSERT INTO configured_git_remote_mint
             (mint_id, command_id, command_kind, storage_version,
              workspace_root, remote_name, remote_url)
         VALUES ($1, $2, 'goal', 1, $3, $4, $5)",
    )
    .bind(mint_id(1))
    .bind(command_id(1))
    .bind(WORKSPACE)
    .bind(NAME)
    .bind(URL)
    .execute(&mut *transaction)
    .await;

    assert!(
        refused.is_err(),
        "a mint row states its own command kind and no other"
    );
    Ok(())
}

/// The composite foreign key, isolated from every other constraint.
///
/// The mint row's own kind stays valid and the durable command is a different
/// kind, so only `(command_id, command_kind, storage_version)` can refuse it.
/// Forcing that one constraint immediate keeps the deferred typed-record
/// trigger from being what fails, so this test also fails if the key is
/// weakened back to a command-ID-only reference.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_mint_bound_to_a_command_of_another_kind_is_refused() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;

    let mut transaction = pool.begin().await?;
    insert_command(&mut transaction, command_id(1), "goal").await?;
    insert_mint(&mut transaction, mint_id(1), command_id(1), WORKSPACE, NAME).await?;

    let refused = sqlx::query("SET CONSTRAINTS configured_git_remote_mint_command_fk IMMEDIATE")
        .execute(&mut *transaction)
        .await;

    assert!(
        refused.is_err(),
        "a mint bound to a command of another kind was admitted"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_mint_naming_no_durable_command_is_refused() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;

    let mut transaction = pool.begin().await?;
    insert_mint(&mut transaction, mint_id(1), command_id(1), WORKSPACE, NAME).await?;

    assert!(
        transaction.commit().await.is_err(),
        "a mint binds to one durable command of its own kind"
    );
    Ok(())
}

async fn name_predicate(pool: &PgPool, candidate: &str) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT configured_git_remote_name_is_valid($1)")
        .bind(candidate)
        .fetch_one(pool)
        .await
}

async fn url_predicate(pool: &PgPool, candidate: &str) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT configured_git_remote_url_is_valid($1)")
        .bind(candidate)
        .fetch_one(pool)
        .await
}

async fn workspace_predicate(pool: &PgPool, candidate: &str) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT configured_git_remote_workspace_is_valid($1)")
        .bind(candidate)
        .fetch_one(pool)
        .await
}

/// Asserts the SQL name predicate returns exactly what the newtype decides.
async fn assert_name_predicate_agrees(
    pool: &PgPool,
    candidate: &str,
) -> Result<(), Box<dyn Error>> {
    assert_eq!(
        name_predicate(pool, candidate).await?,
        GitRemoteName::try_new(candidate.to_owned()).is_ok(),
        "the SQL and Rust name rules disagree about {candidate:?}"
    );
    Ok(())
}

/// Asserts the SQL destination predicate returns exactly what the newtype
/// decides.
async fn assert_url_predicate_agrees(pool: &PgPool, candidate: &str) -> Result<(), Box<dyn Error>> {
    assert_eq!(
        url_predicate(pool, candidate).await?,
        GitRemoteUrl::try_new(candidate.to_owned()).is_ok(),
        "the SQL and Rust destination rules disagree about {candidate:?}"
    );
    Ok(())
}

/// Asserts the SQL workspace predicate returns exactly what the newtype
/// decides.
async fn assert_workspace_predicate_agrees(
    pool: &PgPool,
    candidate: &str,
) -> Result<(), Box<dyn Error>> {
    assert_eq!(
        workspace_predicate(pool, candidate).await?,
        GitRemoteWorkspaceRoot::try_new(candidate.to_owned()).is_ok(),
        "the SQL and Rust workspace rules disagree about {candidate:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn the_name_predicate_agrees_with_the_domain_newtype() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;

    assert_name_predicate_agrees(&pool, "origin").await?;
    assert_name_predicate_agrees(&pool, "up-stream_2").await?;
    assert_name_predicate_agrees(&pool, "v1.0").await?;
    assert_name_predicate_agrees(&pool, "origin.lockfile").await?;
    assert_name_predicate_agrees(&pool, ".origin").await?;
    assert_name_predicate_agrees(&pool, "origin.").await?;
    assert_name_predicate_agrees(&pool, "origin..backup").await?;
    assert_name_predicate_agrees(&pool, "origin.lock").await?;
    assert_name_predicate_agrees(&pool, "namespace/origin").await?;
    assert_name_predicate_agrees(&pool, "..").await?;
    assert_name_predicate_agrees(&pool, ".").await?;
    assert_name_predicate_agrees(&pool, "").await?;
    assert_name_predicate_agrees(&pool, "origin ").await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn the_destination_predicate_agrees_with_the_domain_newtype() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;

    assert_url_predicate_agrees(&pool, "https://example.test/namespace/project.git").await?;
    assert_url_predicate_agrees(&pool, "https://example.test").await?;
    assert_url_predicate_agrees(&pool, "https://a").await?;
    assert_url_predicate_agrees(&pool, "https://example.test:8443/project.git").await?;
    assert_url_predicate_agrees(&pool, "https://user@example.test/project.git").await?;
    assert_url_predicate_agrees(&pool, "https://[2001:db8::1]/project.git").await?;
    assert_url_predicate_agrees(&pool, "https://[....]/project.git").await?;
    assert_url_predicate_agrees(&pool, "https://?").await?;
    assert_url_predicate_agrees(&pool, "https://#fragment").await?;
    assert_url_predicate_agrees(&pool, "https:///project.git").await?;
    assert_url_predicate_agrees(&pool, "https://user@/project.git").await?;
    assert_url_predicate_agrees(&pool, "https://example.test:/project.git").await?;
    assert_url_predicate_agrees(&pool, "https://example.test:https/project.git").await?;
    assert_url_predicate_agrees(&pool, "https://example.test:65535/project.git").await?;
    assert_url_predicate_agrees(&pool, "https://example.test:65536/project.git").await?;
    assert_url_predicate_agrees(&pool, "https://example.test:123456/project.git").await?;
    assert_url_predicate_agrees(&pool, "https://").await?;
    assert_url_predicate_agrees(&pool, "http://example.test/project.git").await?;
    assert_url_predicate_agrees(&pool, "git@example.test:namespace/project.git").await?;
    assert_url_predicate_agrees(&pool, "https://example.test/a project.git").await?;
    assert_url_predicate_agrees(&pool, "https://example.test/a\u{00a0}project.git").await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn the_workspace_predicate_agrees_with_the_domain_newtype() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let longest = format!("/{}", "a".repeat(1023));
    let beyond = format!("/{}", "a".repeat(1024));

    assert_workspace_predicate_agrees(&pool, WORKSPACE).await?;
    assert_workspace_predicate_agrees(&pool, "/").await?;
    assert_workspace_predicate_agrees(&pool, "workspace").await?;
    assert_workspace_predicate_agrees(&pool, "").await?;
    assert_workspace_predicate_agrees(&pool, "/srv/\u{00a0}workspace").await?;
    assert_workspace_predicate_agrees(&pool, longest.as_str()).await?;
    assert_workspace_predicate_agrees(&pool, beyond.as_str()).await?;
    Ok(())
}

/// The workspace bound exists so the composite index tuple stays inside
/// PostgreSQL's B-tree limit; this mints at the longest admitted root and name.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn the_longest_admitted_workspace_root_indexes() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let longest = format!("/{}", "a".repeat(1023));
    let longest_name = "n".repeat(255);

    mint(
        &pool,
        command_id(1),
        mint_id(1),
        longest.as_str(),
        longest_name.as_str(),
    )
    .await?;

    assert_eq!(
        live_mints(&pool).await?,
        1,
        "the bound keeps an index tuple within its limit"
    );
    Ok(())
}
