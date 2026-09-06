#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

//! Pins the durable workspace and Git remote authority schema against the
//! domain newtypes.
//!
//! The migration's `CHECK` predicates restate, in SQL, rules the domain
//! newtypes enforce in Rust. Nothing links the two declarations, so each rule is
//! exercised here through the real constraint with the value the Rust
//! constructor accepted or refused.
//!
//! The one-live-mint rule is carried by a unique constraint over the derived
//! live view rather than by a counting trigger, so the tests below drive it at
//! the isolation levels a counting trigger could not hold.

use std::{error::Error, time::Duration};

use signalbox_domain::{
    DurableCommandId, GitRemoteMintId, GitRemoteName, GitRemoteUrl, GitRemoteWithdrawalId,
    WorkspaceId, WorkspaceRootPath,
};
use signalbox_persistence::{
    disposable_postgres_server_args, disposable_postgres_state_tmpfs_from_example,
    disposable_test_container_labels, local_test_connection_options, migrate,
};
use sqlx::{PgConnection, PgPool, postgres::PgPoolOptions, types::Uuid};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ContainerAsync, ImageExt, runners::AsyncRunner},
};

const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
const DATABASE_NAME: &str = "signalbox_git_remote_authority";
const DATABASE_USER: &str = "signalbox";
const DATABASE_PASSWORD: &str = "signalbox-test-only";

const WORKSPACE_ROOT: &str = "/srv/signalbox/workspace";
const OTHER_WORKSPACE_ROOT: &str = "/srv/signalbox/workspace.sessions/second";
const NAME: &str = "origin";
const URL: &str = "https://example.test/namespace/project.git";

async fn migrated_postgres() -> Result<(ContainerAsync<Postgres>, PgPool), Box<dyn Error>> {
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
    migrate(&pool).await?;
    Ok((container, pool))
}

/// One durable-command identity, distinct per seed and otherwise arbitrary.
///
/// The seed carries no meaning beyond uniqueness. The identity kinds are drawn
/// from disjoint ranges so a transposed same-typed argument names nothing that
/// exists rather than silently naming a row of the other kind.
fn command_id(seed: u128) -> DurableCommandId {
    DurableCommandId::from_uuid(Uuid::from_u128(0xc0de_0000 + seed))
}

/// One mint identity, distinct per seed and otherwise arbitrary.
fn mint_id(seed: u128) -> GitRemoteMintId {
    GitRemoteMintId::from_uuid(Uuid::from_u128(0xa11d_0000 + seed))
}

/// One withdrawal identity, distinct per seed and otherwise arbitrary.
fn withdrawal_id(seed: u128) -> GitRemoteWithdrawalId {
    GitRemoteWithdrawalId::from_uuid(Uuid::from_u128(0xbeef_0000 + seed))
}

/// One workspace identity, distinct per seed and otherwise arbitrary.
fn workspace_id(seed: u128) -> WorkspaceId {
    WorkspaceId::from_uuid(Uuid::from_u128(0x5eed_0000 + seed))
}

/// Records one durable command of the given kind.
async fn insert_command(
    connection: &mut PgConnection,
    command: DurableCommandId,
    kind: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO durable_command (command_id, command_kind, storage_version, claimed_at, issuer_kind)
         VALUES ($1, $2, 1, now(), 'operator')",
    )
    .bind(command.into_uuid())
    .bind(kind)
    .execute(connection)
    .await
    .map(|_| ())
}

/// Records one daemon-derived workspace, which carries no command provenance.
async fn insert_derived_workspace(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    root_path: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO workspace (workspace_id, root_path, origin)
         VALUES ($1, $2, 'daemon_derived')",
    )
    .bind(workspace.into_uuid())
    .bind(root_path)
    .execute(connection)
    .await
    .map(|_| ())
}

/// Records one daemon-derived workspace in its own committed transaction.
async fn derived_workspace(
    pool: &PgPool,
    workspace: WorkspaceId,
    root_path: &str,
) -> Result<(), sqlx::Error> {
    let mut transaction = pool.begin().await?;
    insert_derived_workspace(&mut transaction, workspace, root_path).await?;
    transaction.commit().await
}

/// Records one mint row bound to an already-recorded command and workspace.
async fn insert_mint(
    connection: &mut PgConnection,
    mint: GitRemoteMintId,
    command: DurableCommandId,
    workspace: WorkspaceId,
    remote_name: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO configured_git_remote_mint
             (mint_id, command_id, command_kind, storage_version,
              workspace_id, remote_name, remote_url)
         VALUES ($1, $2, 'mint_git_remote', 1, $3, $4, $5)",
    )
    .bind(mint.into_uuid())
    .bind(command.into_uuid())
    .bind(workspace.into_uuid())
    .bind(remote_name)
    .bind(URL)
    .execute(connection)
    .await
    .map(|_| ())
}

/// Records one withdrawal of an already-recorded mint.
async fn insert_withdrawal(
    connection: &mut PgConnection,
    withdrawal: GitRemoteWithdrawalId,
    mint: GitRemoteMintId,
    command: DurableCommandId,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO configured_git_remote_withdrawal
             (withdrawal_id, mint_id, command_id, command_kind, storage_version)
         VALUES ($1, $2, $3, 'withdraw_git_remote', 1)",
    )
    .bind(withdrawal.into_uuid())
    .bind(mint.into_uuid())
    .bind(command.into_uuid())
    .execute(connection)
    .await
    .map(|_| ())
}

/// Mints one destination in its own committed transaction.
async fn mint(
    pool: &PgPool,
    command: DurableCommandId,
    mint: GitRemoteMintId,
    workspace: WorkspaceId,
    remote_name: &str,
) -> Result<(), sqlx::Error> {
    let mut transaction = pool.begin().await?;
    insert_command(&mut transaction, command, "mint_git_remote").await?;
    insert_mint(&mut transaction, mint, command, workspace, remote_name).await?;
    transaction.commit().await
}

/// Counts the rows standing in the derived live view.
async fn live_mints(pool: &PgPool) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT count(*) FROM configured_git_remote_live")
        .fetch_one(pool)
        .await
}

/// Counts mints the append-only facts show as un-withdrawn.
///
/// The live view is what carries the uniqueness rule, and the facts are what it
/// is derived from. Asserting both keeps a test from passing because the view
/// was empty when the facts said otherwise.
async fn un_withdrawn_mints(pool: &PgPool) -> Result<i64, sqlx::Error> {
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

/// Asserts the live view and the facts agree on how many mints stand.
#[track_caller]
fn assert_counts_agree(live: i64, un_withdrawn: i64, expected: i64, context: &str) {
    assert_eq!(live, expected, "the live view disagrees: {context}");
    assert_eq!(
        un_withdrawn, expected,
        "the minting facts disagree: {context}"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_minted_remote_commits_through_the_durable_command_registry() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    derived_workspace(&pool, workspace_id(1), WORKSPACE_ROOT).await?;

    mint(&pool, command_id(1), mint_id(1), workspace_id(1), NAME).await?;

    assert_counts_agree(
        live_mints(&pool).await?,
        un_withdrawn_mints(&pool).await?,
        1,
        "the mint kind reaches the typed-record registry",
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_later_mint_for_one_workspace_and_name_is_refused() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    derived_workspace(&pool, workspace_id(1), WORKSPACE_ROOT).await?;
    mint(&pool, command_id(1), mint_id(1), workspace_id(1), NAME).await?;

    let refused = mint(&pool, command_id(2), mint_id(2), workspace_id(1), NAME).await;

    assert!(
        refused.is_err(),
        "one workspace and name resolve to at most one live destination"
    );
    assert_counts_agree(
        live_mints(&pool).await?,
        un_withdrawn_mints(&pool).await?,
        1,
        "the refused mint left the first destination standing alone",
    );
    Ok(())
}

/// The rule is scoped to one workspace, so the same name is mintable again for
/// a different one. This is what distinguishes a per-workspace rule from a
/// global one, and it fails if the uniqueness constraint loses its
/// `workspace_id` column.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn one_name_is_mintable_once_per_workspace() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    derived_workspace(&pool, workspace_id(1), WORKSPACE_ROOT).await?;
    derived_workspace(&pool, workspace_id(2), OTHER_WORKSPACE_ROOT).await?;

    mint(&pool, command_id(1), mint_id(1), workspace_id(1), NAME).await?;
    mint(&pool, command_id(2), mint_id(2), workspace_id(2), NAME).await?;

    assert_counts_agree(
        live_mints(&pool).await?,
        un_withdrawn_mints(&pool).await?,
        2,
        "one name stands live once in each of two workspaces",
    );
    Ok(())
}

/// A mint must name a workspace that exists. Without the foreign key an
/// authority grant could be scoped to an identity nothing ever registered.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_mint_naming_no_workspace_is_refused() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;

    let refused = mint(&pool, command_id(1), mint_id(1), workspace_id(9), NAME).await;

    assert!(
        refused.is_err(),
        "a mint was scoped to a workspace that was never registered"
    );
    Ok(())
}

/// A snapshot taken before the winner commits cannot see the winning row, so a
/// counting trigger would have admitted both mints here. The unique constraint
/// consults the index instead, which no snapshot hides, and the second commit
/// fails. This is the whole reason the live view exists.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_mint_is_refused_under_repeatable_read_after_a_concurrent_winner()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    derived_workspace(&pool, workspace_id(1), WORKSPACE_ROOT).await?;

    let mut contender = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *contender)
        .await?;
    // Force the snapshot before the winner commits, so the contender's later
    // reads genuinely predate it.
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM configured_git_remote_live")
        .fetch_one(&mut *contender)
        .await?;

    mint(&pool, command_id(1), mint_id(1), workspace_id(1), NAME).await?;

    insert_command(&mut contender, command_id(2), "mint_git_remote").await?;
    insert_mint(
        &mut contender,
        mint_id(2),
        command_id(2),
        workspace_id(1),
        NAME,
    )
    .await?;

    assert!(
        contender.commit().await.is_err(),
        "a second live destination committed from a snapshot that predated the first"
    );
    assert_counts_agree(
        live_mints(&pool).await?,
        un_withdrawn_mints(&pool).await?,
        1,
        "the winning mint stands alone",
    );
    Ok(())
}

/// A lone mint under `REPEATABLE READ` is admitted: the constraint has no
/// advisory-lock dependency on snapshot refresh, so this fails if an
/// isolation-level refusal is introduced.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_mint_under_repeatable_read_is_admitted() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    derived_workspace(&pool, workspace_id(1), WORKSPACE_ROOT).await?;

    let mut transaction = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *transaction)
        .await?;
    insert_command(&mut transaction, command_id(1), "mint_git_remote").await?;
    insert_mint(
        &mut transaction,
        mint_id(1),
        command_id(1),
        workspace_id(1),
        NAME,
    )
    .await?;
    transaction.commit().await?;

    assert_counts_agree(
        live_mints(&pool).await?,
        un_withdrawn_mints(&pool).await?,
        1,
        "a mint written outside READ COMMITTED was refused",
    );
    Ok(())
}

/// Two transactions holding uncommitted mints of one name must not both commit.
/// The deferred constraint waits on the other transaction rather than reading
/// past it, so exactly one survives.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
/// Both commits are polled together. The deferred key is rechecked at commit,
/// so the first committer blocks on the other transaction's uncommitted index
/// entry until that transaction resolves; awaiting the commits in sequence
/// leaves the second one unpolled and the test hangs rather than failing.
async fn only_one_of_two_simultaneous_mints_for_one_name_commits() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    derived_workspace(&pool, workspace_id(1), WORKSPACE_ROOT).await?;

    let mut holder = pool.begin().await?;
    insert_command(&mut holder, command_id(1), "mint_git_remote").await?;
    insert_mint(
        &mut holder,
        mint_id(1),
        command_id(1),
        workspace_id(1),
        NAME,
    )
    .await?;

    let mut contender = pool.begin().await?;
    insert_command(&mut contender, command_id(2), "mint_git_remote").await?;
    insert_mint(
        &mut contender,
        mint_id(2),
        command_id(2),
        workspace_id(1),
        NAME,
    )
    .await?;

    let (held, contended) = tokio::join!(
        tokio::time::timeout(Duration::from_secs(10), holder.commit()),
        tokio::time::timeout(Duration::from_secs(10), contender.commit()),
    );
    let held = held?;
    let contended = contended?;

    assert!(
        held.is_ok() != contended.is_ok(),
        "exactly one of two simultaneous mints of one name must commit"
    );
    assert_counts_agree(
        live_mints(&pool).await?,
        un_withdrawn_mints(&pool).await?,
        1,
        "exactly one of two simultaneous mints survived",
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_withdrawal_and_its_replacement_land_in_one_transaction() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    derived_workspace(&pool, workspace_id(1), WORKSPACE_ROOT).await?;
    mint(&pool, command_id(1), mint_id(1), workspace_id(1), NAME).await?;

    let mut transaction = pool.begin().await?;
    insert_command(&mut transaction, command_id(2), "withdraw_git_remote").await?;
    insert_withdrawal(
        &mut transaction,
        withdrawal_id(1),
        mint_id(1),
        command_id(2),
    )
    .await?;
    insert_command(&mut transaction, command_id(3), "mint_git_remote").await?;
    insert_mint(
        &mut transaction,
        mint_id(2),
        command_id(3),
        workspace_id(1),
        NAME,
    )
    .await?;
    transaction.commit().await?;

    assert_counts_agree(
        live_mints(&pool).await?,
        un_withdrawn_mints(&pool).await?,
        1,
        "the replacement is the sole live destination",
    );
    Ok(())
}

/// The replacement may also be written before the withdrawal that frees the
/// name. The constraint is deferred precisely so the store is not forced into
/// one statement order.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_replacement_may_precede_its_withdrawal_in_one_transaction() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;
    derived_workspace(&pool, workspace_id(1), WORKSPACE_ROOT).await?;
    mint(&pool, command_id(1), mint_id(1), workspace_id(1), NAME).await?;

    let mut transaction = pool.begin().await?;
    insert_command(&mut transaction, command_id(2), "mint_git_remote").await?;
    insert_mint(
        &mut transaction,
        mint_id(2),
        command_id(2),
        workspace_id(1),
        NAME,
    )
    .await?;
    insert_command(&mut transaction, command_id(3), "withdraw_git_remote").await?;
    insert_withdrawal(
        &mut transaction,
        withdrawal_id(1),
        mint_id(1),
        command_id(3),
    )
    .await?;
    transaction.commit().await?;

    assert_counts_agree(
        live_mints(&pool).await?,
        un_withdrawn_mints(&pool).await?,
        1,
        "the replacement written first is the sole live destination",
    );
    Ok(())
}

/// A withdrawal retires its mint from the live view, freeing the name.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_withdrawn_mint_stops_standing_live() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    derived_workspace(&pool, workspace_id(1), WORKSPACE_ROOT).await?;
    mint(&pool, command_id(1), mint_id(1), workspace_id(1), NAME).await?;

    let mut transaction = pool.begin().await?;
    insert_command(&mut transaction, command_id(2), "withdraw_git_remote").await?;
    insert_withdrawal(
        &mut transaction,
        withdrawal_id(1),
        mint_id(1),
        command_id(2),
    )
    .await?;
    transaction.commit().await?;

    assert_counts_agree(
        live_mints(&pool).await?,
        un_withdrawn_mints(&pool).await?,
        0,
        "the withdrawn destination still stood",
    );
    mint(&pool, command_id(3), mint_id(2), workspace_id(1), NAME).await?;
    assert_counts_agree(
        live_mints(&pool).await?,
        un_withdrawn_mints(&pool).await?,
        1,
        "the freed name was not mintable again",
    );
    Ok(())
}

/// The live view carries the uniqueness rule, so it must be provably derived.
/// A row filed under a name its own mint never named is what would disarm it:
/// the rule is keyed by `(workspace_id, remote_name)`, so a mis-scoped row
/// frees the real name while the destination still stands.
///
/// The legitimate row is deleted and the mis-scoped replacement inserted inside
/// one transaction. Inserting the replacement alongside the legitimate row
/// instead is refused immediately by the unique key on `mint_id`, and the
/// deferred scope check this test exists to pin would never run — the test
/// would pass while proving nothing about that branch.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_live_row_misstating_its_mint_scope_is_refused() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    derived_workspace(&pool, workspace_id(1), WORKSPACE_ROOT).await?;
    mint(&pool, command_id(1), mint_id(1), workspace_id(1), NAME).await?;

    let mut transaction = pool.begin().await?;
    sqlx::query("DELETE FROM configured_git_remote_live WHERE mint_id = $1")
        .bind(mint_id(1).into_uuid())
        .execute(&mut *transaction)
        .await?;
    sqlx::query(
        "INSERT INTO configured_git_remote_live (workspace_id, remote_name, mint_id)
         VALUES ($1, 'backup', $2)",
    )
    .bind(workspace_id(1).into_uuid())
    .bind(mint_id(1).into_uuid())
    .execute(&mut *transaction)
    .await?;

    assert!(
        transaction.commit().await.is_err(),
        "a live row misstating a mint's scope was admitted"
    );
    assert_counts_agree(
        live_mints(&pool).await?,
        un_withdrawn_mints(&pool).await?,
        1,
        "the destination still stands after the refused rewrite",
    );
    Ok(())
}

/// A live row naming a mint that never existed is refused by the foreign key,
/// before the derivation check is reached. Pinning it separately keeps the
/// scope test above honest about which guard each case exercises.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_live_row_naming_no_mint_is_refused() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    derived_workspace(&pool, workspace_id(1), WORKSPACE_ROOT).await?;

    let mut transaction = pool.begin().await?;
    let refused = sqlx::query(
        "INSERT INTO configured_git_remote_live (workspace_id, remote_name, mint_id)
         VALUES ($1, 'backup', $2)",
    )
    .bind(workspace_id(1).into_uuid())
    .bind(mint_id(9).into_uuid())
    .execute(&mut *transaction)
    .await;

    assert!(
        refused.is_err(),
        "a live row naming no mint at all was admitted"
    );
    Ok(())
}

/// A derived workspace carries no command provenance at all, and the `CHECK`
/// spells both legal shapes out rather than comparing `origin` against
/// "provenance is present". Under that shorter equality a row carrying only
/// `command_id` made both sides false and passed, and the composite foreign
/// key's default `MATCH SIMPLE` then skipped validation because one of its
/// columns was null — admitting exactly the partial provenance the schema says
/// a derived row cannot carry.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_derived_workspace_carrying_partial_command_provenance_is_refused()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;

    let mut transaction = pool.begin().await?;
    insert_command(&mut transaction, command_id(1), "register_workspace").await?;
    let refused = sqlx::query(
        "INSERT INTO workspace (workspace_id, root_path, origin, command_id)
         VALUES ($1, $2, 'daemon_derived', $3)",
    )
    .bind(workspace_id(1).into_uuid())
    .bind(WORKSPACE_ROOT)
    .bind(command_id(1).into_uuid())
    .execute(&mut *transaction)
    .await;

    assert!(
        refused.is_err(),
        "a derived workspace carrying partial command provenance was admitted"
    );
    Ok(())
}

/// Deleting a live row for a mint no withdrawal names would silently free the
/// name while the facts still show the destination standing.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn retiring_a_live_row_without_a_withdrawal_is_refused() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    derived_workspace(&pool, workspace_id(1), WORKSPACE_ROOT).await?;
    mint(&pool, command_id(1), mint_id(1), workspace_id(1), NAME).await?;

    let mut transaction = pool.begin().await?;
    sqlx::query("DELETE FROM configured_git_remote_live WHERE mint_id = $1")
        .bind(mint_id(1).into_uuid())
        .execute(&mut *transaction)
        .await?;

    assert!(
        transaction.commit().await.is_err(),
        "a live row was retired with no withdrawal behind it"
    );
    assert_counts_agree(
        live_mints(&pool).await?,
        un_withdrawn_mints(&pool).await?,
        1,
        "the destination still stands after the refused retirement",
    );
    Ok(())
}

/// The row-local `CHECK` pins a mint row's own discriminator.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_mint_row_stating_another_command_kind_is_refused() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    derived_workspace(&pool, workspace_id(1), WORKSPACE_ROOT).await?;

    let mut transaction = pool.begin().await?;
    let refused = sqlx::query(
        "INSERT INTO configured_git_remote_mint
             (mint_id, command_id, command_kind, storage_version,
              workspace_id, remote_name, remote_url)
         VALUES ($1, $2, 'goal', 1, $3, $4, $5)",
    )
    .bind(mint_id(1).into_uuid())
    .bind(command_id(1).into_uuid())
    .bind(workspace_id(1).into_uuid())
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
    derived_workspace(&pool, workspace_id(1), WORKSPACE_ROOT).await?;

    let mut transaction = pool.begin().await?;
    insert_command(&mut transaction, command_id(1), "goal").await?;
    insert_mint(
        &mut transaction,
        mint_id(1),
        command_id(1),
        workspace_id(1),
        NAME,
    )
    .await?;

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
    derived_workspace(&pool, workspace_id(1), WORKSPACE_ROOT).await?;

    let mut transaction = pool.begin().await?;
    insert_mint(
        &mut transaction,
        mint_id(1),
        command_id(1),
        workspace_id(1),
        NAME,
    )
    .await?;

    assert!(
        transaction.commit().await.is_err(),
        "a mint binds to one durable command of its own kind"
    );
    Ok(())
}

/// An operator-registered workspace is a human act and carries the durable
/// command that registered it, reaching the typed-record registry the same way
/// every other command-bound row does.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn an_operator_registered_workspace_commits_through_its_command() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;

    let mut transaction = pool.begin().await?;
    insert_command(&mut transaction, command_id(1), "register_workspace").await?;
    sqlx::query(
        "INSERT INTO workspace
             (workspace_id, root_path, origin, command_id, command_kind, storage_version)
         VALUES ($1, $2, 'operator_registered', $3, 'register_workspace', 1)",
    )
    .bind(workspace_id(1).into_uuid())
    .bind(WORKSPACE_ROOT)
    .bind(command_id(1).into_uuid())
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;

    let registered: i64 =
        sqlx::query_scalar("SELECT count(*) FROM workspace WHERE origin = 'operator_registered'")
            .fetch_one(&pool)
            .await?;

    assert_eq!(registered, 1, "the registered workspace did not commit");
    Ok(())
}

/// The origin and the command provenance are one fact stated twice, so a
/// derived row claiming a command is refused.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_derived_workspace_claiming_command_provenance_is_refused() -> Result<(), Box<dyn Error>>
{
    let (_container, pool) = migrated_postgres().await?;

    let mut transaction = pool.begin().await?;
    insert_command(&mut transaction, command_id(1), "register_workspace").await?;
    let refused = sqlx::query(
        "INSERT INTO workspace
             (workspace_id, root_path, origin, command_id, command_kind, storage_version)
         VALUES ($1, $2, 'daemon_derived', $3, 'register_workspace', 1)",
    )
    .bind(workspace_id(1).into_uuid())
    .bind(WORKSPACE_ROOT)
    .bind(command_id(1).into_uuid())
    .execute(&mut *transaction)
    .await;

    assert!(
        refused.is_err(),
        "a daemon-derived workspace claimed command provenance"
    );
    Ok(())
}

/// The same rule in the other direction: a new authority scope cannot be
/// registered without the human act that authorized it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn an_operator_registered_workspace_without_a_command_is_refused()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;

    let mut transaction = pool.begin().await?;
    let refused = sqlx::query(
        "INSERT INTO workspace (workspace_id, root_path, origin)
         VALUES ($1, $2, 'operator_registered')",
    )
    .bind(workspace_id(1).into_uuid())
    .bind(WORKSPACE_ROOT)
    .execute(&mut *transaction)
    .await;

    assert!(
        refused.is_err(),
        "a new authority scope was registered with no command behind it"
    );
    Ok(())
}

/// One directory is one workspace. Without this the identity above the path
/// stops being unique and two grants could stand for the same directory.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn two_workspaces_cannot_share_one_root() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    derived_workspace(&pool, workspace_id(1), WORKSPACE_ROOT).await?;

    let refused = derived_workspace(&pool, workspace_id(2), WORKSPACE_ROOT).await;

    assert!(
        refused.is_err(),
        "one root was registered as two workspaces"
    );
    Ok(())
}

/// Asserts one aliasing spelling of [`WORKSPACE_ROOT`] dies at the durable
/// boundary rather than at some later comparison.
async fn assert_aliasing_root_is_refused(
    pool: &PgPool,
    workspace: WorkspaceId,
    alias: &str,
) -> Result<(), Box<dyn Error>> {
    let refused = derived_workspace(pool, workspace, alias).await;

    assert!(
        refused.is_err(),
        "the workspace table admitted the aliasing spelling {alias:?}"
    );
    Ok(())
}

/// Aliasing spellings a path-keyed scope would admit as distinct. Each names the
/// same directory as [`WORKSPACE_ROOT`], and each must die at the durable
/// boundary rather than at some later comparison.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn a_workspace_root_aliasing_another_spelling_is_refused() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;

    assert_aliasing_root_is_refused(&pool, workspace_id(1), "/srv/signalbox/workspace/.").await?;
    assert_aliasing_root_is_refused(&pool, workspace_id(2), "/srv/signalbox/nested/../workspace")
        .await?;
    assert_aliasing_root_is_refused(&pool, workspace_id(3), "/srv//signalbox/workspace").await?;
    assert_aliasing_root_is_refused(&pool, workspace_id(4), "/srv/signalbox/workspace/").await?;
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
    sqlx::query_scalar("SELECT workspace_root_path_is_valid($1)")
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
        WorkspaceRootPath::try_new(candidate.to_owned()).is_ok(),
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
    assert_url_predicate_agrees(&pool, "https://1.2.3.999/repo").await?;
    assert_url_predicate_agrees(&pool, "https://4294967296/repo").await?;
    assert_url_predicate_agrees(&pool, "https://1.2.65536/repo").await?;
    assert_url_predicate_agrees(&pool, "https://1.16777216/repo").await?;
    assert_url_predicate_agrees(&pool, "https://1.2.3.4.5/repo").await?;
    assert_url_predicate_agrees(&pool, "https://256.1/repo").await?;
    assert_url_predicate_agrees(&pool, "https://09/repo").await?;
    assert_url_predicate_agrees(&pool, "https://1.09/repo").await?;
    assert_url_predicate_agrees(&pool, "https://0x100000000/repo").await?;
    assert_url_predicate_agrees(&pool, "https://example.1/repo").await?;
    assert_url_predicate_agrees(&pool, "https://1..2/repo").await?;
    assert_url_predicate_agrees(&pool, "https://1.2.3.999./repo").await?;
    assert_url_predicate_agrees(&pool, "https://1/repo").await?;
    assert_url_predicate_agrees(&pool, "https://4294967295/repo").await?;
    assert_url_predicate_agrees(&pool, "https://1.2.65535/repo").await?;
    assert_url_predicate_agrees(&pool, "https://1.16777215/repo").await?;
    assert_url_predicate_agrees(&pool, "https://127.0.0.1/repo").await?;
    assert_url_predicate_agrees(&pool, "https://127.1./repo").await?;
    assert_url_predicate_agrees(&pool, "https://0x7f.1/repo").await?;
    assert_url_predicate_agrees(&pool, "https://0177.1/repo").await?;
    assert_url_predicate_agrees(&pool, "https://0x/repo").await?;
    assert_url_predicate_agrees(&pool, "https://0xFFFFFFFF/repo").await?;
    assert_url_predicate_agrees(&pool, "https://1.0x/repo").await?;
    assert_url_predicate_agrees(&pool, "https://example.0xg/repo").await?;
    assert_url_predicate_agrees(&pool, "https://example%2etest/repo").await?;
    assert_url_predicate_agrees(&pool, r"https://example.test\repo").await?;

    assert_url_predicate_agrees(&pool, "https://a").await?;
    assert_url_predicate_agrees(&pool, "https://example.test:8443/project.git").await?;
    assert_url_predicate_agrees(&pool, "https://user@example.test/project.git").await?;
    assert_url_predicate_agrees(&pool, "https://user:token@example.test/project.git").await?;
    assert_url_predicate_agrees(&pool, "https://@example.test/project.git").await?;
    assert_url_predicate_agrees(&pool, "https://user@example.test:8443/project.git").await?;
    assert_url_predicate_agrees(
        &pool,
        "https://example.test/project.git?access_token=secret",
    )
    .await?;
    assert_url_predicate_agrees(&pool, "https://example.test/project.git?a=1").await?;
    assert_url_predicate_agrees(&pool, "https://example.test?a=1").await?;
    assert_url_predicate_agrees(&pool, "https://example.test/project.git#fragment").await?;
    assert_url_predicate_agrees(&pool, "https://example.test:0/project.git").await?;
    assert_url_predicate_agrees(&pool, "https://example.test:00000/project.git").await?;
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
    assert_url_predicate_agrees(&pool, "https://example.test:00001/project.git").await?;
    assert_url_predicate_agrees(&pool, "https://example.test:000001/project.git").await?;
    assert_url_predicate_agrees(&pool, "https://example.test:0000000001/project.git").await?;
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

    assert_workspace_predicate_agrees(&pool, WORKSPACE_ROOT).await?;
    assert_workspace_predicate_agrees(&pool, OTHER_WORKSPACE_ROOT).await?;
    assert_workspace_predicate_agrees(&pool, "/srv/proyectos/año/workspace").await?;
    assert_workspace_predicate_agrees(&pool, "/").await?;
    assert_workspace_predicate_agrees(&pool, "workspace").await?;
    assert_workspace_predicate_agrees(&pool, "").await?;
    assert_workspace_predicate_agrees(&pool, "/srv/signalbox/workspace/.").await?;
    assert_workspace_predicate_agrees(&pool, "/srv/signalbox/workspace/..").await?;
    assert_workspace_predicate_agrees(&pool, "/srv/signalbox/nested/../workspace").await?;
    assert_workspace_predicate_agrees(&pool, "/./workspace").await?;
    assert_workspace_predicate_agrees(&pool, "/../workspace").await?;
    assert_workspace_predicate_agrees(&pool, "/srv//signalbox").await?;
    assert_workspace_predicate_agrees(&pool, "/srv/signalbox/workspace/").await?;
    assert_workspace_predicate_agrees(&pool, "/..").await?;
    assert_workspace_predicate_agrees(&pool, "/.").await?;
    assert_workspace_predicate_agrees(&pool, "/srv/..workspace").await?;
    assert_workspace_predicate_agrees(&pool, "/srv/...").await?;
    assert_workspace_predicate_agrees(&pool, "/srv/\u{00a0}workspace").await?;
    assert_workspace_predicate_agrees(&pool, longest.as_str()).await?;
    assert_workspace_predicate_agrees(&pool, beyond.as_str()).await?;
    Ok(())
}

/// The workspace bound exists so the unique index tuple stays inside
/// PostgreSQL's B-tree limit; this registers at the longest admitted root and
/// mints at the longest admitted name.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn the_longest_admitted_workspace_root_indexes() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let longest = format!("/{}", "a".repeat(1023));
    let longest_name = "n".repeat(255);

    derived_workspace(&pool, workspace_id(1), longest.as_str()).await?;
    mint(
        &pool,
        command_id(1),
        mint_id(1),
        workspace_id(1),
        longest_name.as_str(),
    )
    .await?;

    assert_counts_agree(
        live_mints(&pool).await?,
        un_withdrawn_mints(&pool).await?,
        1,
        "the bound keeps an index tuple within its limit",
    );
    Ok(())
}

/// Attempts one mint carrying the given text, returning the database's verdict.
///
/// The agreement tests call the predicate functions directly, which cannot see
/// a `CHECK` that was dropped or attached to the wrong column. These drive the
/// real table so the durable boundary itself is what refuses the value.
async fn mint_through_the_table(
    pool: &PgPool,
    remote_name: &str,
    remote_url: &str,
) -> Result<(), sqlx::Error> {
    let mut transaction = pool.begin().await?;
    insert_command(&mut transaction, command_id(1), "mint_git_remote").await?;
    sqlx::query(
        "INSERT INTO configured_git_remote_mint
             (mint_id, command_id, command_kind, storage_version,
              workspace_id, remote_name, remote_url)
         VALUES ($1, $2, 'mint_git_remote', 1, $3, $4, $5)",
    )
    .bind(mint_id(1).into_uuid())
    .bind(command_id(1).into_uuid())
    .bind(workspace_id(1).into_uuid())
    .bind(remote_name)
    .bind(remote_url)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn the_table_refuses_a_name_the_newtype_refuses() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    derived_workspace(&pool, workspace_id(1), WORKSPACE_ROOT).await?;

    let refused = mint_through_the_table(&pool, ".origin", URL).await;

    assert!(
        refused.is_err(),
        "the mint table admitted a name GitRemoteName refuses"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn the_table_refuses_a_destination_the_newtype_refuses() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    derived_workspace(&pool, workspace_id(1), WORKSPACE_ROOT).await?;

    let refused = mint_through_the_table(&pool, NAME, "https://?").await;

    assert!(
        refused.is_err(),
        "the mint table admitted a destination GitRemoteUrl refuses"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn the_table_refuses_a_workspace_root_the_newtype_refuses() -> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;

    let refused = derived_workspace(&pool, workspace_id(1), "workspace").await;

    assert!(
        refused.is_err(),
        "the workspace table admitted a root WorkspaceRootPath refuses"
    );
    Ok(())
}
