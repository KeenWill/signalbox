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
//! plus one hop of body references from those functions — rather than
//! matching rendered definition text, so a function reached only through a
//! user-defined operator is still found, and a migration that adds an
//! unpinned reachable function fails here instead of failing the next
//! restore. Each pin must carry the canonical value — the migration-selected
//! schema, then `pg_catalog`, then `pg_temp` — because a pin that omits the
//! working schema fails restore exactly like a missing pin. One
//! body-reference hop is a deliberate limit: deeper call chains have no
//! mechanical catalogue representation, and the schema's current chains are
//! one deep. The test also fails when discovery returns nothing: the schema's
//! check constraints do reach functions, so an empty set means the discovery
//! query broke, not that nothing needs pinning.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

use std::error::Error;

use signalbox_persistence::{
    disposable_postgres_server_args, disposable_postgres_state_tmpfs_from_example,
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
    WITH restore_dependency AS (
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
        SELECT oid, proname, proconfig FROM reachable
        UNION
        SELECT callee.oid, callee.proname, callee.proconfig
          FROM pg_proc AS callee
          JOIN reachable AS caller
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
