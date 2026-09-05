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
//! representation, so the closure follows call-shaped `prosrc` references
//! until no new function appears, and a chain of unqualified calls is followed
//! to its end rather than one hop deep. The lexical classifier recognizes
//! quoted identifiers and comments between a function name and its opening
//! parenthesis while excluding names inside comments and strings and bare
//! aliases. The test also fails when discovery returns nothing: the schema's
//! check constraints do reach functions, so an empty set means the discovery
//! query broke, not that nothing needs pinning.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

use std::{collections::BTreeSet, error::Error};

use signalbox_persistence::{
    disposable_postgres_server_args, disposable_postgres_state_tmpfs_from_example,
    disposable_test_container_labels, local_test_connection_options, migrate,
};
use sqlx::{PgPool, postgres::PgPoolOptions};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ImageExt, runners::AsyncRunner},
};

const POSTGRES_IMAGE_TAG: &str = "18.4-alpine3.23";
const DATABASE_NAME: &str = "signalbox_search_path";
const DATABASE_USER: &str = "signalbox";
const DATABASE_PASSWORD: &str = "signalbox-test-only";

const RESTORE_ROOT_FUNCTIONS: &str = "
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
    )
    SELECT DISTINCT root.oid::bigint
      FROM (
            SELECT dependency.refobjid AS oid
              FROM restore_dependency AS dependency
             WHERE dependency.refclassid = 'pg_proc'::regclass
            UNION
            SELECT operator.oprcode::oid AS oid
              FROM restore_dependency AS dependency
              JOIN pg_operator AS operator ON operator.oid = dependency.refobjid
             WHERE dependency.refclassid = 'pg_operator'::regclass
      ) AS root
      JOIN pg_proc AS function ON function.oid = root.oid
     WHERE function.pronamespace =
           (SELECT oid FROM pg_namespace WHERE nspname = current_schema())
     ORDER BY 1
";

const RESTORE_SCHEMA_FUNCTIONS: &str = "
    SELECT oid::bigint,
           proname,
           EXISTS (
               SELECT 1
                 FROM unnest(coalesce(proconfig, ARRAY[]::text[])) AS cfg
                WHERE cfg = format(
                          'search_path=%I, pg_catalog, pg_temp',
                          current_schema()
                      )
           ) AS pinned,
           prosrc
      FROM pg_proc
     WHERE pronamespace =
           (SELECT oid FROM pg_namespace WHERE nspname = current_schema())
     ORDER BY oid
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
                LANGUAGE sql IMMUTABLE AS
                'SELECT {RESTORE_PROBE_TAIL}()'"
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

#[derive(Clone, Debug, Eq, PartialEq)]
struct RestoreFunction {
    oid: i64,
    name: String,
    pinned: bool,
    source: String,
}

async fn restore_reachable_functions(pool: &PgPool) -> Result<Vec<(String, bool)>, sqlx::Error> {
    let roots: Vec<i64> = sqlx::query_scalar(RESTORE_ROOT_FUNCTIONS)
        .fetch_all(pool)
        .await?;
    let catalogue: Vec<(i64, String, bool, String)> = sqlx::query_as(RESTORE_SCHEMA_FUNCTIONS)
        .fetch_all(pool)
        .await?;
    let catalogue = catalogue
        .into_iter()
        .map(|(oid, name, pinned, source)| RestoreFunction {
            oid,
            name,
            pinned,
            source,
        })
        .collect::<Vec<_>>();
    Ok(restore_reachable_function_pins(&roots, &catalogue))
}

fn restore_reachable_function_pins(
    roots: &[i64],
    catalogue: &[RestoreFunction],
) -> Vec<(String, bool)> {
    let calls = catalogue
        .iter()
        .map(|function| (function.oid, body_call_names(&function.source)))
        .collect::<Vec<_>>();
    let mut covered = roots.iter().copied().collect::<BTreeSet<_>>();
    loop {
        let referenced = calls
            .iter()
            .filter(|(oid, _)| covered.contains(oid))
            .flat_map(|(_, names)| names.iter())
            .collect::<BTreeSet<_>>();
        let discovered = catalogue
            .iter()
            .filter(|function| !covered.contains(&function.oid))
            .filter(|function| referenced.contains(&function.name))
            .map(|function| function.oid)
            .collect::<Vec<_>>();
        if discovered.is_empty() {
            break;
        }
        covered.extend(discovered);
    }
    let mut reachable = catalogue
        .iter()
        .filter(|function| covered.contains(&function.oid))
        .map(|function| (function.name.clone(), function.pinned, function.oid))
        .collect::<Vec<_>>();
    reachable.sort_unstable_by(|left, right| {
        (left.0.as_str(), left.2).cmp(&(right.0.as_str(), right.2))
    });
    reachable
        .into_iter()
        .map(|(name, pinned, _)| (name, pinned))
        .collect()
}

fn body_call_names(source: &str) -> BTreeSet<String> {
    let bytes = source.as_bytes();
    let mut cursor = 0;
    let mut preceding_identifier = None;
    let mut calls = BTreeSet::new();
    while cursor < bytes.len() {
        cursor = skip_sql_trivia(bytes, cursor);
        if cursor >= bytes.len() {
            break;
        }
        match bytes[cursor] {
            b'\'' => {
                cursor = skip_single_quoted(bytes, cursor);
                preceding_identifier = None;
            }
            b'$' => {
                if let Some(after) = skip_dollar_quoted(source, cursor) {
                    cursor = after;
                    preceding_identifier = None;
                } else {
                    cursor += 1;
                    preceding_identifier = None;
                }
            }
            b'"' => {
                let (identifier, after) = quoted_identifier(source, cursor);
                cursor = after;
                preceding_identifier = identifier;
            }
            byte if is_identifier_start(byte) => {
                let start = cursor;
                cursor += 1;
                while cursor < bytes.len() && is_identifier_continue(bytes[cursor]) {
                    cursor += 1;
                }
                preceding_identifier = Some(source[start..cursor].to_ascii_lowercase());
            }
            b'(' => {
                if let Some(identifier) = preceding_identifier.take() {
                    calls.insert(identifier);
                }
                cursor += 1;
            }
            _ => {
                cursor += 1;
                preceding_identifier = None;
            }
        }
    }
    calls
}

const fn is_identifier_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

const fn is_identifier_continue(byte: u8) -> bool {
    is_identifier_start(byte) || byte.is_ascii_digit() || byte == b'$'
}

fn skip_sql_trivia(bytes: &[u8], mut cursor: usize) -> usize {
    loop {
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if bytes.get(cursor..cursor + 2) == Some(b"--") {
            cursor += 2;
            while cursor < bytes.len() && !matches!(bytes[cursor], b'\n' | b'\r') {
                cursor += 1;
            }
            continue;
        }
        if bytes.get(cursor..cursor + 2) != Some(b"/*") {
            return cursor;
        }
        cursor += 2;
        let mut depth = 1_u32;
        while cursor < bytes.len() && depth > 0 {
            if bytes.get(cursor..cursor + 2) == Some(b"/*") {
                depth += 1;
                cursor += 2;
            } else if bytes.get(cursor..cursor + 2) == Some(b"*/") {
                depth -= 1;
                cursor += 2;
            } else {
                cursor += 1;
            }
        }
    }
}

fn skip_single_quoted(bytes: &[u8], mut cursor: usize) -> usize {
    cursor += 1;
    while cursor < bytes.len() {
        if bytes[cursor] == b'\\' {
            cursor = (cursor + 2).min(bytes.len());
        } else if bytes[cursor] == b'\'' && bytes.get(cursor + 1) == Some(&b'\'') {
            cursor += 2;
        } else if bytes[cursor] == b'\'' {
            return cursor + 1;
        } else {
            cursor += 1;
        }
    }
    cursor
}

fn skip_dollar_quoted(source: &str, cursor: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut delimiter_end = cursor + 1;
    if bytes.get(delimiter_end) != Some(&b'$') {
        if !bytes
            .get(delimiter_end)
            .copied()
            .is_some_and(is_identifier_start)
        {
            return None;
        }
        delimiter_end += 1;
        while bytes
            .get(delimiter_end)
            .copied()
            .is_some_and(is_dollar_tag_continue)
        {
            delimiter_end += 1;
        }
    }
    if bytes.get(delimiter_end) != Some(&b'$') {
        return None;
    }
    let delimiter = &source[cursor..=delimiter_end];
    let content_start = delimiter_end + 1;
    source[content_start..]
        .find(delimiter)
        .map(|offset| content_start + offset + delimiter.len())
        .or(Some(source.len()))
}

const fn is_dollar_tag_continue(byte: u8) -> bool {
    is_identifier_start(byte) || byte.is_ascii_digit()
}

fn quoted_identifier(source: &str, mut cursor: usize) -> (Option<String>, usize) {
    let bytes = source.as_bytes();
    cursor += 1;
    let mut segment_start = cursor;
    let mut identifier = String::new();
    while cursor < bytes.len() {
        if bytes[cursor] != b'"' {
            cursor += 1;
            continue;
        }
        identifier.push_str(&source[segment_start..cursor]);
        if bytes.get(cursor + 1) == Some(&b'"') {
            identifier.push('"');
            cursor += 2;
            segment_start = cursor;
        } else {
            return (Some(identifier), cursor + 1);
        }
    }
    (None, cursor)
}

/// Names of covered functions that lack the canonical pin.
fn unpinned_names(covered: &[(String, bool)]) -> Vec<&str> {
    covered
        .iter()
        .filter(|(_, pinned)| !pinned)
        .map(|(name, _)| name.as_str())
        .collect()
}

/// every function reachable from a check constraint or index during
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

    let covered = restore_reachable_functions(&pool).await?;
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

/// body-reference discovery closes transitively — a check constraint
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

    let covered = restore_reachable_functions(&pool).await?;
    assert_eq!(
        unpinned_names(&covered),
        [RESTORE_PROBE_HEAD, RESTORE_PROBE_MIDDLE, RESTORE_PROBE_TAIL],
        "the probe chain must surface: head directly, middle one body hop deep, \
         and tail two hops deep, which only a transitive closure reaches"
    );
    Ok(())
}

#[test]
fn quoted_function_identifier_is_a_call_edge() {
    assert_eq!(
        body_call_names(r#"SELECT "restore_probe_tail"()"#),
        BTreeSet::from([String::from(RESTORE_PROBE_TAIL)])
    );
}

#[test]
fn block_comment_between_function_name_and_parenthesis_preserves_the_call_edge() {
    assert_eq!(
        body_call_names("SELECT restore_probe_tail /* nested /* body */ comment */ ()"),
        BTreeSet::from([String::from(RESTORE_PROBE_TAIL)])
    );
}

#[test]
fn line_comment_between_function_name_and_parenthesis_preserves_the_call_edge() {
    assert_eq!(
        body_call_names("SELECT restore_probe_tail -- body comment\n ()"),
        BTreeSet::from([String::from(RESTORE_PROBE_TAIL)])
    );
}

#[test]
fn same_spelled_bare_alias_is_not_a_call_edge() {
    assert!(body_call_names("SELECT true AS restore_probe_tail").is_empty());
}

#[test]
fn call_shaped_name_inside_a_comment_is_not_a_call_edge() {
    assert!(body_call_names("SELECT true /* restore_probe_tail() */").is_empty());
}

#[test]
fn call_shaped_name_inside_a_string_is_not_a_call_edge() {
    assert!(body_call_names("SELECT 'restore_probe_tail()'").is_empty());
}

#[test]
fn call_shaped_name_inside_a_dollar_quoted_string_is_not_a_call_edge() {
    assert!(body_call_names("SELECT $body$restore_probe_tail()$body$").is_empty());
}
