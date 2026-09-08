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
//! aliases, including explicit column aliases and CTE column lists. Strings
//! use PostgreSQL's standard-conforming semantics unless prefixed with `E`.
//! Dynamic SQL inside strings, Unicode escape identifiers, and non-ASCII
//! unquoted identifiers are outside this lexical discovery's supported syntax.
//! The test also fails when discovery returns nothing: the schema's
//! check constraints do reach functions, so an empty set means the discovery
//! query broke, not that nothing needs pinning.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "this standalone integration-test crate uses assertion panics and explicit fixture expectations; the workspace gate remains active for production targets"
)]

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
};

use signalbox_persistence::{
    disposable_postgres_server_args, disposable_postgres_state_tmpfs_from_example,
    disposable_test_container_labels, local_test_connection_options, migrate,
};
use sqlx::{PgPool, postgres::PgPoolOptions};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{ImageExt, runners::AsyncRunner},
};

#[path = "../../../tooling/postgres_test_image.rs"]
mod postgres_test_image;
use postgres_test_image::POSTGRES_IMAGE_TAG;
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
                'SELECT result FROM (SELECT 1, CASE {RESTORE_PROBE_TAIL}() WHEN true THEN true ELSE false END AS result) AS probe'"
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
    let keywords = postgres_keywords(pool).await?;
    Ok(restore_reachable_function_pins(
        &roots, &catalogue, &keywords,
    ))
}

async fn postgres_keywords(pool: &PgPool) -> Result<BTreeMap<String, KeywordUse>, sqlx::Error> {
    let keywords: Vec<(String, bool, bool)> = sqlx::query_as(
        "SELECT word, catcode IN ('U', 'C'), catcode IN ('U', 'T') FROM pg_get_keywords()",
    )
    .fetch_all(pool)
    .await?;
    Ok(keywords
        .into_iter()
        .map(|(word, relation_name, function_name)| {
            (
                word,
                KeywordUse {
                    relation_name,
                    function_name,
                },
            )
        })
        .collect())
}

fn restore_reachable_function_pins(
    roots: &[i64],
    catalogue: &[RestoreFunction],
    keywords: &BTreeMap<String, KeywordUse>,
) -> Vec<(String, bool)> {
    let calls = catalogue
        .iter()
        .map(|function| (function.oid, body_call_names(&function.source, keywords)))
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

fn body_call_names(source: &str, keywords: &BTreeMap<String, KeywordUse>) -> BTreeSet<String> {
    let tokens = body_tokens(source, keywords);
    let cte_headers = cte_header_positions(&tokens);
    let relation_aliases = relation_alias_positions(&tokens);
    tokens
        .windows(2)
        .enumerate()
        .filter_map(|(index, pair)| {
            let previous = index.checked_sub(1).map(|previous| &tokens[previous]);
            let name = match (&pair[0], previous) {
                (BodyToken::Word { name, .. }, Some(BodyToken::Dot)) => name.as_str(),
                _ => pair[0].function_name()?,
            };
            (pair[1] == BodyToken::Open
                && !cte_headers.contains(&index)
                && !relation_aliases.contains(&index)
                && !previous
                    .is_some_and(|token| token.is_keyword("as") || *token == BodyToken::Close))
            .then(|| name.to_owned())
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct KeywordUse {
    relation_name: bool,
    function_name: bool,
}

#[derive(Debug, PartialEq)]
enum BodyToken {
    Word {
        name: String,
        keyword: Option<KeywordUse>,
    },
    Dot,
    Open,
    Close,
    Comma,
    Star,
    Other,
}

impl BodyToken {
    fn is_keyword(&self, word: &str) -> bool {
        matches!(self, Self::Word { name, keyword: Some(_) } if name == word)
    }

    fn is_relation_name(&self) -> bool {
        matches!(
            self,
            Self::Word {
                keyword: None
                    | Some(KeywordUse {
                        relation_name: true,
                        ..
                    }),
                ..
            }
        )
    }

    fn function_name(&self) -> Option<&str> {
        match self {
            Self::Word {
                name,
                keyword:
                    None
                    | Some(KeywordUse {
                        function_name: true,
                        ..
                    }),
            } => Some(name),
            _ => None,
        }
    }
}

fn body_tokens(source: &str, keywords: &BTreeMap<String, KeywordUse>) -> Vec<BodyToken> {
    let bytes = source.as_bytes();
    let mut cursor = 0;
    let mut tokens = Vec::new();
    while cursor < bytes.len() {
        cursor = skip_sql_trivia(bytes, cursor);
        if cursor >= bytes.len() {
            break;
        }
        match bytes[cursor] {
            b'e' | b'E' if bytes.get(cursor + 1) == Some(&b'\'') => {
                cursor = skip_single_quoted(bytes, cursor + 1, StringSyntax::Escape);
                tokens.push(BodyToken::Other);
            }
            b'\'' => {
                cursor = skip_single_quoted(bytes, cursor, StringSyntax::Standard);
                tokens.push(BodyToken::Other);
            }
            b'$' => {
                if let Some(after) = skip_dollar_quoted(source, cursor) {
                    cursor = after;
                } else {
                    cursor += 1;
                }
                tokens.push(BodyToken::Other);
            }
            b'"' => {
                let (identifier, after) = quoted_identifier(source, cursor);
                cursor = after;
                tokens.push(identifier.map_or(BodyToken::Other, |name| BodyToken::Word {
                    name,
                    keyword: None,
                }));
            }
            byte if is_identifier_start(byte) => {
                let start = cursor;
                cursor += 1;
                while cursor < bytes.len() && is_identifier_continue(bytes[cursor]) {
                    cursor += 1;
                }
                let identifier = source[start..cursor].to_ascii_lowercase();
                let keyword = keywords.get(&identifier).copied();
                tokens.push(BodyToken::Word {
                    name: identifier,
                    keyword,
                });
            }
            punctuation @ (b'(' | b')' | b',' | b'.' | b'*') => {
                tokens.push(match punctuation {
                    b'(' => BodyToken::Open,
                    b')' => BodyToken::Close,
                    b'.' => BodyToken::Dot,
                    b'*' => BodyToken::Star,
                    _ => BodyToken::Comma,
                });
                cursor += 1;
            }
            _ => {
                cursor += 1;
                tokens.push(BodyToken::Other);
            }
        }
    }
    tokens
}

fn relation_alias_positions(tokens: &[BodyToken]) -> BTreeSet<usize> {
    let mut aliases = BTreeSet::new();
    for (index, token) in tokens.iter().enumerate() {
        if !token.is_keyword("from") && !token.is_keyword("join") {
            continue;
        }
        let mut cursor = index + 1;
        loop {
            while tokens
                .get(cursor)
                .is_some_and(|token| token.is_keyword("only") || token.is_keyword("lateral"))
            {
                cursor += 1;
            }
            if tokens.get(cursor) != Some(&BodyToken::Open) {
                let source_name = tokens.get(cursor).is_some_and(|token| {
                    token.is_relation_name()
                        || (token.function_name().is_some()
                            && tokens.get(cursor + 1) == Some(&BodyToken::Open))
                });
                if !source_name {
                    break;
                }
                cursor += 1;
                while tokens.get(cursor) == Some(&BodyToken::Dot)
                    && matches!(tokens.get(cursor + 1), Some(BodyToken::Word { .. }))
                {
                    cursor += 2;
                }
            }
            if tokens.get(cursor) == Some(&BodyToken::Open) {
                let Some(after) = after_parenthesized(tokens, cursor) else {
                    break;
                };
                cursor = after;
            }
            if tokens.get(cursor) == Some(&BodyToken::Star) {
                cursor += 1;
            }
            if tokens
                .get(cursor)
                .is_some_and(|token| token.is_keyword("with"))
                && tokens
                    .get(cursor + 1)
                    .is_some_and(|token| token.is_keyword("ordinality"))
            {
                cursor += 2;
            }
            if tokens
                .get(cursor)
                .is_some_and(|token| token.is_keyword("as"))
            {
                cursor += 1;
            }
            if tokens.get(cursor).is_some_and(BodyToken::is_relation_name) {
                aliases.insert(cursor);
                cursor += 1;
                if tokens.get(cursor) == Some(&BodyToken::Open) {
                    let Some(after) = after_parenthesized(tokens, cursor) else {
                        break;
                    };
                    cursor = after;
                }
            }
            if tokens.get(cursor) != Some(&BodyToken::Comma) {
                break;
            }
            cursor += 1;
        }
    }
    aliases
}

fn cte_header_positions(tokens: &[BodyToken]) -> BTreeSet<usize> {
    let mut headers = BTreeSet::new();
    for (index, token) in tokens.iter().enumerate() {
        if !token.is_keyword("with") {
            continue;
        }
        let mut cursor = index + 1;
        if tokens
            .get(cursor)
            .is_some_and(|token| token.is_keyword("recursive"))
        {
            cursor += 1;
        }
        while tokens.get(cursor).is_some_and(BodyToken::is_relation_name) {
            let name = cursor;
            cursor += 1;
            if tokens.get(cursor) == Some(&BodyToken::Open) {
                let Some(after) = after_parenthesized(tokens, cursor) else {
                    break;
                };
                cursor = after;
            }
            if !tokens
                .get(cursor)
                .is_some_and(|token| token.is_keyword("as"))
            {
                break;
            }
            cursor += 1;
            if tokens
                .get(cursor)
                .is_some_and(|token| token.is_keyword("not"))
            {
                cursor += 1;
            }
            if tokens
                .get(cursor)
                .is_some_and(|token| token.is_keyword("materialized"))
            {
                cursor += 1;
            }
            let Some(after) = after_parenthesized(tokens, cursor) else {
                break;
            };
            headers.extend(name..cursor);
            let Some(after) = after_cte_search_and_cycle(tokens, after) else {
                break;
            };
            cursor = after;
            if tokens.get(cursor) != Some(&BodyToken::Comma) {
                break;
            }
            cursor += 1;
        }
    }
    headers
}

fn after_cte_search_and_cycle(tokens: &[BodyToken], mut cursor: usize) -> Option<usize> {
    if tokens
        .get(cursor)
        .is_some_and(|token| token.is_keyword("search"))
    {
        cursor += 1;
        if !tokens
            .get(cursor)
            .is_some_and(|token| token.is_keyword("breadth") || token.is_keyword("depth"))
        {
            return None;
        }
        cursor += 1;
        for keyword in ["first", "by"] {
            if !tokens
                .get(cursor)
                .is_some_and(|token| token.is_keyword(keyword))
            {
                return None;
            }
            cursor += 1;
        }
        cursor = after_cte_column_list_and_mark(tokens, cursor)?;
    }
    if tokens
        .get(cursor)
        .is_some_and(|token| token.is_keyword("cycle"))
    {
        cursor = after_cte_column_list_and_mark(tokens, cursor + 1)?;
        if tokens
            .get(cursor)
            .is_some_and(|token| token.is_keyword("to"))
        {
            // CYCLE mark/default values are AexprConst in PostgreSQL's grammar.
            // Skip their type modifiers as balanced groups, including commas.
            cursor += 1;
            while !tokens.get(cursor)?.is_keyword("using") {
                if tokens.get(cursor) == Some(&BodyToken::Open) {
                    cursor = after_parenthesized(tokens, cursor)?;
                } else {
                    cursor += 1;
                }
            }
        }
        if !tokens
            .get(cursor)
            .is_some_and(|token| token.is_keyword("using"))
            || !tokens
                .get(cursor + 1)
                .is_some_and(BodyToken::is_relation_name)
        {
            return None;
        }
        cursor += 2;
    }
    Some(cursor)
}

fn after_cte_column_list_and_mark(tokens: &[BodyToken], mut cursor: usize) -> Option<usize> {
    loop {
        if !tokens.get(cursor).is_some_and(BodyToken::is_relation_name) {
            return None;
        }
        cursor += 1;
        if tokens.get(cursor) != Some(&BodyToken::Comma) {
            break;
        }
        cursor += 1;
    }
    if !tokens
        .get(cursor)
        .is_some_and(|token| token.is_keyword("set"))
        || !tokens
            .get(cursor + 1)
            .is_some_and(BodyToken::is_relation_name)
    {
        return None;
    }
    Some(cursor + 2)
}

fn after_parenthesized(tokens: &[BodyToken], start: usize) -> Option<usize> {
    if tokens.get(start) != Some(&BodyToken::Open) {
        return None;
    }
    let mut depth = 0;
    for (index, token) in tokens.iter().enumerate().skip(start) {
        match token {
            BodyToken::Open => depth += 1,
            BodyToken::Close => {
                depth -= 1;
                if depth == 0 {
                    return Some(index + 1);
                }
            }
            _ => {}
        }
    }
    None
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

enum StringSyntax {
    Standard,
    Escape,
}

fn skip_single_quoted(bytes: &[u8], mut cursor: usize, syntax: StringSyntax) -> usize {
    cursor += 1;
    while cursor < bytes.len() {
        if matches!(syntax, StringSyntax::Escape) && bytes[cursor] == b'\\' {
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
    let keywords = postgres_keywords(&pool).await?;
    let classifier_failures = collect_classifier_failures(&keywords);
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
    let unpinned = unpinned_names(&covered);
    assert!(
        classifier_failures.is_empty()
            && unpinned == [RESTORE_PROBE_HEAD, RESTORE_PROBE_MIDDLE, RESTORE_PROBE_TAIL],
        "classifier failures: {classifier_failures:?}; fixed-point reachability: {unpinned:?}; \
         expected head, middle and tail"
    );
    Ok(())
}

fn collect_classifier_failures(keywords: &BTreeMap<String, KeywordUse>) -> Vec<&'static str> {
    type ClassifierCheck = fn(&BTreeMap<String, KeywordUse>);
    let checks: &[(&str, ClassifierCheck)] = &[
        (
            "quoted_function_identifier_is_a_call_edge",
            quoted_function_identifier_is_a_call_edge,
        ),
        (
            "block_comment_between_function_name_and_parenthesis_preserves_the_call_edge",
            block_comment_between_function_name_and_parenthesis_preserves_the_call_edge,
        ),
        (
            "line_comment_between_function_name_and_parenthesis_preserves_the_call_edge",
            line_comment_between_function_name_and_parenthesis_preserves_the_call_edge,
        ),
        (
            "same_spelled_bare_alias_is_not_a_call_edge",
            same_spelled_bare_alias_is_not_a_call_edge,
        ),
        (
            "call_shaped_name_inside_a_comment_is_not_a_call_edge",
            call_shaped_name_inside_a_comment_is_not_a_call_edge,
        ),
        (
            "call_shaped_name_inside_a_string_is_not_a_call_edge",
            call_shaped_name_inside_a_string_is_not_a_call_edge,
        ),
        (
            "call_shaped_name_inside_a_dollar_quoted_string_is_not_a_call_edge",
            call_shaped_name_inside_a_dollar_quoted_string_is_not_a_call_edge,
        ),
        (
            "standard_string_backslash_does_not_hide_following_calls",
            standard_string_backslash_does_not_hide_following_calls,
        ),
        (
            "escape_strings_skip_escaped_quotes_without_inventing_calls",
            escape_strings_skip_escaped_quotes_without_inventing_calls,
        ),
        (
            "doubled_standard_quotes_keep_call_shaped_string_content_hidden",
            doubled_standard_quotes_keep_call_shaped_string_content_hidden,
        ),
        (
            "column_alias_lists_do_not_add_call_edges",
            column_alias_lists_do_not_add_call_edges,
        ),
        (
            "table_alias_column_lists_do_not_add_call_edges",
            table_alias_column_lists_do_not_add_call_edges,
        ),
        (
            "function_sources_remain_call_edges",
            function_sources_remain_call_edges,
        ),
        (
            "cte_column_lists_preserve_calls_inside_each_body",
            cte_column_lists_preserve_calls_inside_each_body,
        ),
        (
            "recursive_cte_clauses_preserve_following_cte_headers",
            recursive_cte_clauses_preserve_following_cte_headers,
        ),
        (
            "parenthesized_body_ends_after_nested_groups",
            parenthesized_body_ends_after_nested_groups,
        ),
        (
            "unterminated_parenthesized_body_has_no_end",
            unterminated_parenthesized_body_has_no_end,
        ),
        (
            "expression_keywords_do_not_turn_calls_into_relation_aliases",
            expression_keywords_do_not_turn_calls_into_relation_aliases,
        ),
        (
            "keyword_categories_preserve_permitted_names_and_quoted_identifiers",
            keyword_categories_preserve_permitted_names_and_quoted_identifiers,
        ),
    ];
    checks
        .iter()
        .filter_map(|(name, check)| {
            std::panic::catch_unwind(|| check(keywords))
                .err()
                .map(|_| *name)
        })
        .collect()
}

fn quoted_function_identifier_is_a_call_edge(keywords: &BTreeMap<String, KeywordUse>) {
    let fixture_body_call_names = |source: &str| body_call_names(source, keywords);
    assert_eq!(
        fixture_body_call_names(r#"SELECT "restore_probe_tail"()"#),
        BTreeSet::from([String::from(RESTORE_PROBE_TAIL)])
    );
}

fn block_comment_between_function_name_and_parenthesis_preserves_the_call_edge(
    keywords: &BTreeMap<String, KeywordUse>,
) {
    let fixture_body_call_names = |source: &str| body_call_names(source, keywords);
    assert_eq!(
        fixture_body_call_names("SELECT restore_probe_tail /* nested /* body */ comment */ ()"),
        BTreeSet::from([String::from(RESTORE_PROBE_TAIL)])
    );
}

fn line_comment_between_function_name_and_parenthesis_preserves_the_call_edge(
    keywords: &BTreeMap<String, KeywordUse>,
) {
    let fixture_body_call_names = |source: &str| body_call_names(source, keywords);
    assert_eq!(
        fixture_body_call_names("SELECT restore_probe_tail -- body comment\n ()"),
        BTreeSet::from([String::from(RESTORE_PROBE_TAIL)])
    );
}

fn same_spelled_bare_alias_is_not_a_call_edge(keywords: &BTreeMap<String, KeywordUse>) {
    let fixture_body_call_names = |source: &str| body_call_names(source, keywords);
    assert!(fixture_body_call_names("SELECT true AS restore_probe_tail").is_empty());
}

fn call_shaped_name_inside_a_comment_is_not_a_call_edge(keywords: &BTreeMap<String, KeywordUse>) {
    let fixture_body_call_names = |source: &str| body_call_names(source, keywords);
    assert!(fixture_body_call_names("SELECT true /* restore_probe_tail() */").is_empty());
}

fn call_shaped_name_inside_a_string_is_not_a_call_edge(keywords: &BTreeMap<String, KeywordUse>) {
    let fixture_body_call_names = |source: &str| body_call_names(source, keywords);
    assert!(fixture_body_call_names("SELECT 'restore_probe_tail()'").is_empty());
}

fn call_shaped_name_inside_a_dollar_quoted_string_is_not_a_call_edge(
    keywords: &BTreeMap<String, KeywordUse>,
) {
    let fixture_body_call_names = |source: &str| body_call_names(source, keywords);
    assert!(fixture_body_call_names("SELECT $body$restore_probe_tail()$body$").is_empty());
}

fn standard_string_backslash_does_not_hide_following_calls(
    keywords: &BTreeMap<String, KeywordUse>,
) {
    let fixture_body_call_names = |source: &str| body_call_names(source, keywords);
    assert_eq!(
        fixture_body_call_names(r"SELECT '\', restore_probe_tail()"),
        BTreeSet::from([String::from(RESTORE_PROBE_TAIL)])
    );
}

fn escape_strings_skip_escaped_quotes_without_inventing_calls(
    keywords: &BTreeMap<String, KeywordUse>,
) {
    let fixture_body_call_names = |source: &str| body_call_names(source, keywords);
    for source in [
        r"SELECT E'it\'s restore_probe_head()', restore_probe_tail()",
        r"SELECT e'it\'s restore_probe_head()', restore_probe_tail()",
        r"SELECT E'\\', restore_probe_tail()",
    ] {
        assert_eq!(
            fixture_body_call_names(source),
            BTreeSet::from([String::from(RESTORE_PROBE_TAIL)]),
            "{source}"
        );
    }
}

fn doubled_standard_quotes_keep_call_shaped_string_content_hidden(
    keywords: &BTreeMap<String, KeywordUse>,
) {
    let fixture_body_call_names = |source: &str| body_call_names(source, keywords);
    assert_eq!(
        fixture_body_call_names("SELECT 'it''s restore_probe_head()', restore_probe_tail()"),
        BTreeSet::from([String::from(RESTORE_PROBE_TAIL)])
    );
}

fn column_alias_lists_do_not_add_call_edges(keywords: &BTreeMap<String, KeywordUse>) {
    let fixture_body_call_names = |source: &str| body_call_names(source, keywords);
    for source in [
        "SELECT * FROM restore_probe_tail() AS restore_probe_head(value)",
        r#"SELECT * FROM restore_probe_tail() AS "restore_probe_head"(value)"#,
        "SELECT * FROM restore_probe_tail() restore_probe_head(value)",
        "SELECT * FROM restore_probe_tail() WITH ORDINALITY restore_probe_head(value, ordinal)",
        "SELECT * FROM (SELECT restore_probe_tail()) AS restore_probe_head(value)",
    ] {
        assert_eq!(
            fixture_body_call_names(source),
            BTreeSet::from([String::from(RESTORE_PROBE_TAIL)]),
            "{source}"
        );
    }
}

fn table_alias_column_lists_do_not_add_call_edges(keywords: &BTreeMap<String, KeywordUse>) {
    let fixture_body_call_names = |source: &str| body_call_names(source, keywords);
    for source in [
        "SELECT restore_probe_tail() FROM records restore_probe_head(value)",
        "SELECT restore_probe_tail() FROM records * restore_probe_head(value)",
        "SELECT restore_probe_tail() FROM public.records * restore_probe_head(value)",
        "SELECT restore_probe_tail() FROM first, records * restore_probe_head(value)",
        "SELECT restore_probe_tail() FROM first JOIN public.records * restore_probe_head(value) ON true",
        "SELECT restore_probe_tail() FROM ONLY records restore_probe_head(value)",
        "SELECT restore_probe_tail() FROM ONLY public.records restore_probe_head(value)",
        "SELECT restore_probe_tail() FROM records JOIN ONLY public.records restore_probe_head(value) ON true",
        "SELECT restore_probe_tail() FROM first, second restore_probe_head(value)",
        "SELECT restore_probe_tail() FROM first, public.second restore_probe_head(value)",
        "SELECT restore_probe_tail() FROM public.records restore_probe_head(value)",
        "SELECT restore_probe_tail() FROM records JOIN public.records restore_probe_head(value) ON true",
    ] {
        assert_eq!(
            fixture_body_call_names(source),
            BTreeSet::from([String::from(RESTORE_PROBE_TAIL)]),
            "{source}"
        );
    }
}

fn function_sources_remain_call_edges(keywords: &BTreeMap<String, KeywordUse>) {
    let fixture_body_call_names = |source: &str| body_call_names(source, keywords);
    for source in [
        "SELECT * FROM public.restore_probe_tail()",
        "SELECT * FROM LATERAL restore_probe_tail()",
        "SELECT * FROM records CROSS JOIN LATERAL restore_probe_tail()",
        "SELECT * FROM records, LATERAL restore_probe_tail()",
    ] {
        assert_eq!(
            fixture_body_call_names(source),
            BTreeSet::from([String::from(RESTORE_PROBE_TAIL)]),
            "{source}",
        );
    }
}

fn cte_column_lists_preserve_calls_inside_each_body(keywords: &BTreeMap<String, KeywordUse>) {
    let fixture_body_call_names = |source: &str| body_call_names(source, keywords);
    assert_eq!(
        fixture_body_call_names(
            "WITH RECURSIVE restore_probe_head(value) AS MATERIALIZED (
                 SELECT restore_probe_tail()
             ), restore_probe_middle(value) AS NOT MATERIALIZED (
                 SELECT restore_probe_tail() FROM restore_probe_head
             ) SELECT * FROM restore_probe_middle"
        ),
        BTreeSet::from([String::from(RESTORE_PROBE_TAIL)])
    );
}

fn recursive_cte_clauses_preserve_following_cte_headers(keywords: &BTreeMap<String, KeywordUse>) {
    for clause in [
        "SEARCH DEPTH FIRST BY n SET ord",
        "SEARCH BREADTH FIRST BY n, set SET ord",
        "CYCLE n, set SET mark USING path",
        "CYCLE n SET mark TO true DEFAULT false USING path",
        "CYCLE n SET mark TO numeric(4, 1) '1' DEFAULT numeric(4, 1) '0' USING path",
        r#"SEARCH DEPTH FIRST BY "set", n SET "cycle" CYCLE "set", n SET mark TO 'using' DEFAULT 'set' USING path"#,
    ] {
        let source = format!(
            "WITH RECURSIVE first(n, set) AS (SELECT restore_probe_tail(), 1) {clause},
             restore_probe_head(v) AS (SELECT restore_probe_tail()),
             restore_probe_middle(v) AS (SELECT restore_probe_tail())
             SELECT * FROM restore_probe_middle"
        );
        assert_eq!(
            body_call_names(&source, keywords),
            BTreeSet::from([String::from(RESTORE_PROBE_TAIL)]),
            "{clause}"
        );
    }
}

fn parenthesized_body_ends_after_nested_groups(keywords: &BTreeMap<String, KeywordUse>) {
    let tokens = body_tokens("(value, (nested)) remaining", keywords);
    assert_eq!(after_parenthesized(&tokens, 0), Some(7));
}

fn unterminated_parenthesized_body_has_no_end(keywords: &BTreeMap<String, KeywordUse>) {
    assert_eq!(
        after_parenthesized(&body_tokens("(value", keywords), 0),
        None
    );
}

fn expression_keywords_do_not_turn_calls_into_relation_aliases(
    keywords: &BTreeMap<String, KeywordUse>,
) {
    let fixture_body_call_names = |source: &str| body_call_names(source, keywords);
    for source in [
        "SELECT 1, CASE WHEN restore_probe_tail() THEN true ELSE false END",
        "SELECT 1, CASE restore_probe_tail() WHEN true THEN true ELSE false END",
        "SELECT CASE true WHEN true THEN restore_probe_tail() ELSE false END",
        "SELECT 1 * restore_probe_tail()",
        "SELECT 1, CURRENT_TIMESTAMP AT TIME ZONE restore_probe_tail()",
        "SELECT value * restore_probe_tail() FROM records",
    ] {
        assert_eq!(
            fixture_body_call_names(source),
            BTreeSet::from([RESTORE_PROBE_TAIL.to_owned()]),
            "{source}"
        );
    }
}

fn keyword_categories_preserve_permitted_names_and_quoted_identifiers(
    keywords: &BTreeMap<String, KeywordUse>,
) {
    let fixture_body_call_names = |source: &str| body_call_names(source, keywords);
    assert!(fixture_body_call_names("SELECT * FROM time restore_probe_head(value)").is_empty());
    assert_eq!(
        fixture_body_call_names("SELECT overlaps(), filter()"),
        BTreeSet::from(["overlaps".to_owned(), "filter".to_owned()])
    );
    for source in [
        "SELECT * FROM overlaps() restore_probe_head(value)",
        "SELECT * FROM overlaps() WITH ORDINALITY restore_probe_head(value, ordinal)",
    ] {
        assert_eq!(
            fixture_body_call_names(source),
            BTreeSet::from(["overlaps".to_owned()]),
            "{source}"
        );
    }
    assert_eq!(
        fixture_body_call_names(r#"SELECT "when"() FROM "case" restore_probe_head(value)"#),
        BTreeSet::from(["when".to_owned()])
    );
    assert_eq!(
        fixture_body_call_names("SELECT public.when() FROM public.case restore_probe_head(value)"),
        BTreeSet::from(["when".to_owned()])
    );
}
