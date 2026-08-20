//! Restore safety of the schema's own functions.
//!
//! `pg_restore` replays a logical backup with an empty search path and
//! evaluates check constraints while copying table data, so every function a
//! constraint or index can reach during restore must carry a pinned search
//! path in its catalogue definition: an unpinned body that names another user
//! function works in normal operation and fails only when the backup is
//! needed. The assertion reads the reachable set from the live catalogue —
//! direct references from check constraints and index expressions, plus one
//! hop of body references from those functions — rather than restating an
//! inventory, so a migration that adds an unpinned reachable function fails
//! here instead of failing the next restore. One body-reference hop is a
//! deliberate limit: deeper call chains have no mechanical catalogue
//! representation, and the schema's current chains are one deep.

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

const UNPINNED_REACHABLE_FUNCTIONS: &str = "
    WITH reachable AS (
        SELECT p.oid, p.proname, p.proconfig, p.prosrc, p.pronamespace
          FROM pg_proc AS p
         WHERE p.pronamespace =
               (SELECT oid FROM pg_namespace WHERE nspname = current_schema())
           AND (
                EXISTS (
                    SELECT 1
                      FROM pg_constraint AS c
                     WHERE c.contype = 'c'
                       AND pg_get_constraintdef(c.oid)
                           ~ ('\\m' || p.proname || '\\M')
                )
                OR EXISTS (
                    SELECT 1
                      FROM pg_index AS i
                      JOIN pg_class AS t ON t.oid = i.indrelid
                     WHERE t.relnamespace = p.pronamespace
                       AND pg_get_indexdef(i.indexrelid)
                           ~ ('\\m' || p.proname || '\\M')
                )
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
    SELECT DISTINCT proname
      FROM covered
     WHERE NOT EXISTS (
            SELECT 1
              FROM unnest(coalesce(proconfig, ARRAY[]::text[])) AS cfg
             WHERE cfg LIKE 'search_path=%'
       )
     ORDER BY proname
";

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

    let unpinned: Vec<String> = sqlx::query_scalar(UNPINNED_REACHABLE_FUNCTIONS)
        .fetch_all(&pool)
        .await?;
    assert!(
        unpinned.is_empty(),
        "restore-reachable functions without a pinned search path: {unpinned:?}"
    );
    Ok(())
}
