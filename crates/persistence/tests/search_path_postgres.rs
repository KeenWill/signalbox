//! Restore safety of the schema's own functions.
//!
//! `pg_restore` replays a logical backup with an empty search path and
//! evaluates check constraints while copying table data, so every function a
//! constraint or index can reach during restore must carry a pinned search
//! path in its catalogue definition: an unpinned body that names another user
//! function works in normal operation and fails only when the backup is
//! needed. The assertion derives the reachable set from the dependency
//! catalogue — the functions that check constraints and indexes record in
//! `pg_depend`, the implementation functions of any operators they record,
//! plus body references followed transitively from those functions — rather
//! than matching rendered definition text, so a function reached only through
//! a user-defined operator is still found, and a migration that adds an
//! unpinned reachable function fails here instead of failing the next
//! restore. Each pin must carry the canonical value — the migration-selected
//! schema, then `pg_catalog`, then `pg_temp` — because a pin that omits the
//! working schema fails restore exactly like a missing pin. Body references
//! close transitively to a fixed point: `pg_depend` has no body-level
//! representation, so the closure follows `prosrc` name references until no
//! new function appears, and a chain of unqualified calls is followed to its
//! end rather than one hop deep. The test also fails when discovery returns
//! nothing: the schema's check constraints do reach functions, so an empty
//! set means the discovery query broke, not that nothing needs pinning.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

use std::error::Error;

use signalbox_persistence::{
    disposable_postgres_server_args, disposable_postgres_state_tmpfs,
    disposable_test_container_labels, local_test_connection_options, migrate,
};
use sqlx::postgres::PgPoolOptions;
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ImageExt, runners::AsyncRunner},
};

const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
const DATABASE_NAME: &str = "signalbox_search_path";
const DATABASE_USER: &str = "signalbox";
const DATABASE_PASSWORD: &str = "signalbox-test-only";

const RESTORE_REACHABLE_FUNCTIONS: &str = "
    WITH RECURSIVE restore_dependency AS (
        SELECT d.refclassid, d.refobjid
          FROM pg_depend AS d
         WHERE (
                d.classid = 'pg_constraint'::regclass
                AND EXISTS (
                    SELECT 1
                      FROM pg_constraint AS c
                     WHERE c.oid = d.objid
                       AND c.contype = 'c'
                )
           )
            OR (
                d.classid = 'pg_class'::regclass
                AND EXISTS (
                    SELECT 1
                      FROM pg_index AS i
                     WHERE i.indexrelid = d.objid
                )
           )
    ),
    reachable AS (
        SELECT p.oid, p.proname, p.proconfig, p.prosrc, p.pronamespace
          FROM pg_proc AS p
         WHERE p.pronamespace =
               (SELECT oid FROM pg_namespace WHERE nspname = current_schema())
           AND p.oid IN (
                SELECT d.refobjid
                  FROM restore_dependency AS d
                 WHERE d.refclassid = 'pg_proc'::regclass
                UNION
                SELECT o.oprcode::oid
                  FROM restore_dependency AS d
                  JOIN pg_operator AS o ON o.oid = d.refobjid
                 WHERE d.refclassid = 'pg_operator'::regclass
           )
    ),
    covered AS (
        SELECT oid, proname, proconfig, prosrc, pronamespace FROM reachable
        UNION
        SELECT callee.oid, callee.proname, callee.proconfig, callee.prosrc,
               callee.pronamespace
          FROM pg_proc AS callee
          JOIN covered AS caller
            ON callee.pronamespace = caller.pronamespace
           AND callee.oid <> caller.oid
           AND caller.prosrc ~ ('\\m' || callee.proname || '\\M')
    )
    SELECT proname,
           EXISTS (
               SELECT 1
                 FROM unnest(coalesce(proconfig, ARRAY[]::text[])) AS cfg
                WHERE cfg = format(
                          'search_path=%I, pg_catalog, pg_temp',
                          current_schema()
                      )
           ) AS pinned
      FROM covered
     ORDER BY proname
";

const RESTORE_PROBE_HEAD: &str = "restore_probe_head";
const RESTORE_PROBE_MIDDLE: &str = "restore_probe_middle";
const RESTORE_PROBE_TAIL: &str = "restore_probe_tail";

/// DDL for a three-deep call chain behind a check constraint, rendered from
/// the probe-name constants so the assertion can never drift from the fixture.
/// The fields are named because the statements must run in tail-to-table
/// dependency order; a positional collection would let a fixture edit
/// silently reorder them.
struct SyntheticTransitiveChain {
    create_tail: String,
    create_middle: String,
    create_head: String,
    create_probe_table: String,
}

fn synthetic_transitive_chain() -> SyntheticTransitiveChain {
    SyntheticTransitiveChain {
        create_tail: format!(
            "CREATE FUNCTION {RESTORE_PROBE_TAIL}() RETURNS boolean
                LANGUAGE sql IMMUTABLE AS 'SELECT true'"
        ),
        create_middle: format!(
            "CREATE FUNCTION {RESTORE_PROBE_MIDDLE}() RETURNS boolean
                LANGUAGE sql IMMUTABLE AS 'SELECT {RESTORE_PROBE_TAIL}()'"
        ),
        create_head: format!(
            "CREATE FUNCTION {RESTORE_PROBE_HEAD}(value text) RETURNS boolean
                LANGUAGE sql IMMUTABLE AS 'SELECT {RESTORE_PROBE_MIDDLE}()'"
        ),
        create_probe_table: format!(
            "CREATE TABLE restore_probe (
                value text,
                CONSTRAINT restore_probe_reaches_functions CHECK ({RESTORE_PROBE_HEAD}(value))
            )"
        ),
    }
}

/// Names of covered functions that lack the canonical pin.
fn unpinned_names(covered: &[(String, bool)]) -> Vec<&str> {
    covered
        .iter()
        .filter(|(_, pinned)| !pinned)
        .map(|(name, _)| name.as_str())
        .collect()
}

/// INV-070: every function reachable from a check constraint or index during
/// `pg_restore` carries the canonical pinned search path — the
/// migration-selected schema, then `pg_catalog`, then `pg_temp`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn every_restore_reachable_function_pins_its_search_path() -> Result<(), Box<dyn Error>> {
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
        .max_connections(2)
        .connect_with(local_test_connection_options(&database_url)?)
        .await?;
    migrate(&pool).await?;

    let covered: Vec<(String, bool)> = sqlx::query_as(RESTORE_REACHABLE_FUNCTIONS)
        .fetch_all(&pool)
        .await?;
    assert!(
        !covered.is_empty(),
        "restore-reachability discovery found no functions, which means the \
         discovery query broke: the schema's check constraints reach functions"
    );
    let unpinned = unpinned_names(&covered);
    assert!(
        unpinned.is_empty(),
        "restore-reachable functions without the canonical search path pin: {unpinned:?}"
    );
    Ok(())
}

/// INV-070: body-reference discovery closes transitively — a check constraint
/// whose function calls through an intermediate body still surfaces the
/// unpinned function at the end of the chain.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn transitive_body_references_close_to_a_fixed_point() -> Result<(), Box<dyn Error>> {
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
        .max_connections(2)
        .connect_with(local_test_connection_options(&database_url)?)
        .await?;
    migrate(&pool).await?;
    let chain = synthetic_transitive_chain();
    sqlx::query(sqlx::AssertSqlSafe(chain.create_tail.as_str()))
        .execute(&pool)
        .await?;
    sqlx::query(sqlx::AssertSqlSafe(chain.create_middle.as_str()))
        .execute(&pool)
        .await?;
    sqlx::query(sqlx::AssertSqlSafe(chain.create_head.as_str()))
        .execute(&pool)
        .await?;
    sqlx::query(sqlx::AssertSqlSafe(chain.create_probe_table.as_str()))
        .execute(&pool)
        .await?;

    let covered: Vec<(String, bool)> = sqlx::query_as(RESTORE_REACHABLE_FUNCTIONS)
        .fetch_all(&pool)
        .await?;
    assert_eq!(
        unpinned_names(&covered),
        [RESTORE_PROBE_HEAD, RESTORE_PROBE_MIDDLE, RESTORE_PROBE_TAIL],
        "the probe chain must surface: head directly, middle one body hop deep, \
         and tail two hops deep, which only a transitive closure reaches"
    );
    Ok(())
}
