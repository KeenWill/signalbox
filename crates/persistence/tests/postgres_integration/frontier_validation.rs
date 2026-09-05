use std::error::Error;

use sqlx::PgPool;
use uuid::Uuid;

use super::{insert_outbox_session_fixture, migrated_postgres};

const DEEP_MEMBER_COUNT: i32 = 1_200;
const PREFIX_MEMBER_COUNT: i32 = 900;
const OBSOLETE_COMPACTION_COUNT: i32 = 256;

/// Boundedness guard on the deep-frontier probes: a starvation allowance for a
/// loaded CI host, not a budget under test. Both probe phases enforce the same
/// guard so their boundedness claims cannot drift apart.
const BOUNDED_PROBE_STATEMENT_TIMEOUT: &str = "SET statement_timeout = '10s'";

async fn insert_deep_frontier_fixture(
    pool: &PgPool,
) -> Result<(Uuid, Uuid, Uuid, Uuid, Uuid), sqlx::Error> {
    let session = insert_outbox_session_fixture(pool, 0xf604).await?;
    let prefix: Uuid = sqlx::query_scalar("SELECT md5('frontier-' || $1)::uuid")
        .bind(PREFIX_MEMBER_COUNT)
        .fetch_one(pool)
        .await?;
    let checked: Uuid = sqlx::query_scalar("SELECT md5('frontier-' || $1)::uuid")
        .bind(DEEP_MEMBER_COUNT)
        .fetch_one(pool)
        .await?;
    let divergent = Uuid::from_u128(0xf604_0001);
    let equivalent = Uuid::from_u128(0xf604_0002);

    sqlx::raw_sql(
        "ALTER TABLE semantic_transcript_entry DISABLE TRIGGER ALL;
         ALTER TABLE context_frontier DISABLE TRIGGER ALL;
         ALTER TABLE context_frontier_delta DISABLE TRIGGER ALL;",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             assistant_text_value, producing_model_call_id,
             assistant_response_part_ordinal,
             assistant_response_text_start_bytes)
         SELECT $1,
                md5('entry-' || member_position)::uuid,
                'assistant_text',
                'fixture member ' || member_position,
                md5('frontier-validation-producing-call')::uuid,
                member_position - 1,
                COALESCE(
                    sum(octet_length('fixture member ' || member_position)) OVER (
                        ORDER BY member_position
                        ROWS BETWEEN UNBOUNDED PRECEDING AND 1 PRECEDING
                    ),
                    0
                )::numeric
           FROM generate_series(1, $2) AS member(member_position)",
    )
    .bind(session)
    .bind(DEEP_MEMBER_COUNT)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO context_frontier
            (owning_session_id, context_frontier_id, member_count,
             prefix_context_frontier_id)
         VALUES ($1, $2, $3, NULL)",
    )
    .bind(session)
    .bind(equivalent)
    .bind(DEEP_MEMBER_COUNT)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO context_frontier_delta
            (owning_session_id, context_frontier_id, member_position,
             source_session_id, semantic_entry_id)
         SELECT $1,
                $2,
                member_position,
                $1,
                md5('entry-' || member_position)::uuid
           FROM generate_series(1, $3) AS member(member_position)",
    )
    .bind(session)
    .bind(equivalent)
    .bind(DEEP_MEMBER_COUNT)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO semantic_transcript_entry
            (source_session_id, semantic_entry_id, payload_kind,
             assistant_text_value, producing_model_call_id,
             assistant_response_part_ordinal,
             assistant_response_text_start_bytes)
         VALUES (
            $1,
            md5('divergent-entry')::uuid,
            'assistant_text',
            'divergent fixture member',
            md5('frontier-validation-producing-call')::uuid,
            $2,
            (
                SELECT sum(octet_length('fixture member ' || member_position))::numeric
                  FROM generate_series(1, $2) AS member(member_position)
            )
         )",
    )
    .bind(session)
    .bind(DEEP_MEMBER_COUNT)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO context_frontier
            (owning_session_id, context_frontier_id, member_count,
             prefix_context_frontier_id)
         SELECT $1,
                md5('frontier-' || member_count)::uuid,
                member_count,
                CASE
                    WHEN member_count = 1 THEN NULL
                    ELSE md5('frontier-' || (member_count - 1))::uuid
                END
           FROM generate_series(1, $2) AS frontier(member_count)",
    )
    .bind(session)
    .bind(DEEP_MEMBER_COUNT)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO context_frontier_delta
            (owning_session_id, context_frontier_id, member_position,
             source_session_id, semantic_entry_id)
         SELECT $1,
                md5('frontier-' || member_position)::uuid,
                member_position,
                $1,
                md5('entry-' || member_position)::uuid
           FROM generate_series(1, $2) AS member(member_position)",
    )
    .bind(session)
    .bind(DEEP_MEMBER_COUNT)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO context_frontier
            (owning_session_id, context_frontier_id, member_count,
             prefix_context_frontier_id)
         VALUES (
            $1,
            $2,
            $3,
            md5('frontier-' || ($4 - 1))::uuid
         )",
    )
    .bind(session)
    .bind(divergent)
    .bind(DEEP_MEMBER_COUNT)
    .bind(PREFIX_MEMBER_COUNT)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO context_frontier_delta
            (owning_session_id, context_frontier_id, member_position,
             source_session_id, semantic_entry_id)
         SELECT $1,
                $2,
                member_position,
                $1,
                CASE
                    WHEN member_position = $3
                    THEN md5('divergent-entry')::uuid
                    ELSE md5('entry-' || member_position)::uuid
                END
           FROM generate_series($3, $4) AS member(member_position)",
    )
    .bind(session)
    .bind(divergent)
    .bind(PREFIX_MEMBER_COUNT)
    .bind(DEEP_MEMBER_COUNT)
    .execute(pool)
    .await?;
    sqlx::raw_sql(
        "ALTER TABLE semantic_transcript_entry ENABLE TRIGGER ALL;
         ALTER TABLE context_frontier ENABLE TRIGGER ALL;
         ALTER TABLE context_frontier_delta ENABLE TRIGGER ALL;",
    )
    .execute(pool)
    .await?;

    Ok((session, prefix, checked, divergent, equivalent))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn deep_frontier_prefix_validation_is_bounded_and_exact() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let (session, prefix, checked, divergent, equivalent) =
        insert_deep_frontier_fixture(&pool).await?;
    let compaction = Uuid::from_u128(0xf605_0001);
    sqlx::raw_sql("ALTER TABLE context_compaction DISABLE TRIGGER ALL;")
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO context_compaction
            (context_compaction_id, session_id, predecessor_compaction_id,
             source_frontier_id, result_frontier_id, producing_call_id,
             first_source_session_id, first_entry_id,
             through_source_session_id, through_entry_id, summary_entry_id)
         VALUES (
            $1, $2, NULL, $3, $4, $5,
            $2, md5('entry-1')::uuid,
            $2, md5('entry-900')::uuid, md5('entry-1')::uuid
         )",
    )
    .bind(compaction)
    .bind(session)
    .bind(divergent)
    .bind(checked)
    .bind(Uuid::from_u128(0xf605_0002))
    .execute(&pool)
    .await?;
    sqlx::raw_sql("ALTER TABLE context_compaction ENABLE TRIGGER ALL;")
        .execute(&pool)
        .await?;
    let mut connection = pool.acquire().await?;
    let validator_shape: (bool, bool, bool, bool, bool, bool, bool, bool, bool) = sqlx::query_as(
        "SELECT
            position(
                'WITH RECURSIVE checked_chain AS MATERIALIZED'
                IN pg_get_functiondef(
                    'context_frontier_preserves_prefix(uuid,uuid,uuid)'::regprocedure
                )
            ) > 0,
            position(
                'ELSE NOT EXISTS'
                IN pg_get_functiondef(
                    'context_frontier_preserves_prefix(uuid,uuid,uuid)'::regprocedure
                )
            ) > 0,
            position(
                'context_frontier_preserves_prefix'
                IN pg_get_functiondef(
                    'assert_model_call_steering_final_state(uuid)'::regprocedure
                )
            ) > 0,
            position(
                'context_frontier_preserves_prefix'
                IN pg_get_functiondef(
                    'continuation_frontier_closes_predecessor_tool_round(uuid,uuid,uuid,uuid)'::regprocedure
                )
            ) > 0,
            position(
                'delegation_result'
                IN pg_get_functiondef(
                    'continuation_frontier_closes_predecessor_tool_round(uuid,uuid,uuid,uuid)'::regprocedure
                )
            ) > 0,
            position(
                'context_frontier_preserves_prefix'
                IN pg_get_functiondef(
                    'turn_start_effective_predecessor_frontier(uuid,uuid)'::regprocedure
                )
            ) > 0,
            position(
                'leaf AS MATERIALIZED'
                IN pg_get_functiondef(
                    'turn_start_effective_predecessor_frontier(uuid,uuid)'::regprocedure
                )
            ) > 0,
            position(
                'candidate.member_count < predecessor.member_count'
                IN pg_get_functiondef(
                    'turn_start_effective_predecessor_frontier(uuid,uuid)'::regprocedure
                )
            ) > 0,
            position(
                'context_frontier_preserves_prefix'
                IN pg_get_functiondef(
                    'assert_terminal_started_turn_common_final_state(uuid)'::regprocedure
                )
            ) > 0",
    )
    .fetch_one(&mut *connection)
    .await?;
    sqlx::query(BOUNDED_PROBE_STATEMENT_TIMEOUT)
        .execute(&mut *connection)
        .await?;

    let preserved: bool =
        sqlx::query_scalar("SELECT context_frontier_preserves_prefix($1, $2, $3)")
            .bind(session)
            .bind(prefix)
            .bind(checked)
            .fetch_one(&mut *connection)
            .await?;
    let rejected: bool = sqlx::query_scalar("SELECT context_frontier_preserves_prefix($1, $2, $3)")
        .bind(session)
        .bind(prefix)
        .bind(divergent)
        .fetch_one(&mut *connection)
        .await?;
    let independently_equivalent: bool =
        sqlx::query_scalar("SELECT context_frontier_preserves_prefix($1, $2, $3)")
            .bind(session)
            .bind(prefix)
            .bind(equivalent)
            .fetch_one(&mut *connection)
            .await?;
    let effective_preserved: Uuid = sqlx::query_scalar(
        "SELECT context_frontier_id
           FROM turn_start_effective_predecessor_frontier($1, $2)",
    )
    .bind(session)
    .bind(prefix)
    .fetch_one(&mut *connection)
    .await?;
    sqlx::raw_sql("ALTER TABLE context_compaction DISABLE TRIGGER ALL;")
        .execute(&mut *connection)
        .await?;
    sqlx::query(
        "UPDATE context_compaction
            SET source_frontier_id = $2, result_frontier_id = $3
          WHERE context_compaction_id = $1",
    )
    .bind(compaction)
    .bind(checked)
    .bind(divergent)
    .execute(&mut *connection)
    .await?;
    sqlx::raw_sql("ALTER TABLE context_compaction ENABLE TRIGGER ALL;")
        .execute(&mut *connection)
        .await?;
    let effective_rejected: Uuid = sqlx::query_scalar(
        "SELECT context_frontier_id
           FROM turn_start_effective_predecessor_frontier($1, $2)",
    )
    .bind(session)
    .bind(prefix)
    .fetch_one(&mut *connection)
    .await?;

    sqlx::query("SET statement_timeout = '0'")
        .execute(&mut *connection)
        .await?;
    sqlx::raw_sql("ALTER TABLE context_compaction DISABLE TRIGGER ALL;")
        .execute(&mut *connection)
        .await?;
    sqlx::query(
        "INSERT INTO context_compaction
            (context_compaction_id, session_id, predecessor_compaction_id,
             source_frontier_id, result_frontier_id, producing_call_id,
             first_source_session_id, first_entry_id,
             through_source_session_id, through_entry_id, summary_entry_id)
         SELECT md5('obsolete-compaction-' || ordinal)::uuid,
                $1,
                CASE
                    WHEN ordinal = 1 THEN $2
                    ELSE md5('obsolete-compaction-' || (ordinal - 1))::uuid
                END,
                CASE
                    WHEN ordinal = 1 THEN $3
                    ELSE md5('frontier-' || ($4 + ordinal - 1))::uuid
                END,
                md5('frontier-' || ($4 + ordinal))::uuid,
                md5('obsolete-producing-call-' || ordinal)::uuid,
                $1,
                md5('entry-1')::uuid,
                $1,
                md5('entry-' || $5)::uuid,
                md5('entry-' || ($4 + ordinal))::uuid
           FROM generate_series(1, $6) AS history(ordinal)",
    )
    .bind(session)
    .bind(compaction)
    .bind(divergent)
    .bind(DEEP_MEMBER_COUNT - OBSOLETE_COMPACTION_COUNT - 1)
    .bind(PREFIX_MEMBER_COUNT)
    .bind(OBSOLETE_COMPACTION_COUNT)
    .execute(&mut *connection)
    .await?;
    sqlx::raw_sql("ALTER TABLE context_compaction ENABLE TRIGGER ALL;")
        .execute(&mut *connection)
        .await?;
    sqlx::query(BOUNDED_PROBE_STATEMENT_TIMEOUT)
        .execute(&mut *connection)
        .await?;
    let effective_after_obsolete_chain: Uuid = sqlx::query_scalar(
        "SELECT context_frontier_id
           FROM turn_start_effective_predecessor_frontier($1, $2)",
    )
    .bind(session)
    .bind(checked)
    .fetch_one(&mut *connection)
    .await?;

    assert_eq!(
        validator_shape,
        (true, true, true, true, true, true, true, true, true)
    );
    assert_eq!(
        (preserved, independently_equivalent, rejected),
        (true, true, false)
    );
    assert_eq!(effective_preserved, checked);
    assert_eq!(effective_rejected, prefix);
    assert_eq!(effective_after_obsolete_chain, checked);

    drop(connection);
    pool.close().await;
    drop(container);
    Ok(())
}

/// compaction validates a successor from its immutable predecessor
/// and bounded typed suffix while retaining root/import compatibility replay.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ephemeral PostgreSQL"]
async fn context_compaction_validation_is_current_and_typed() -> Result<(), Box<dyn Error>> {
    let (container, pool, _database_url) = migrated_postgres().await?;
    let validator_shape: (bool, bool, bool, bool, bool, bool) = sqlx::query_as(
        "SELECT
            position(
                'stored compaction summary leaves a tool exchange open'
                IN pg_get_functiondef(
                    'require_context_compaction_exact_evidence()'::regprocedure
                )
            ) = 0,
            position(
                'entry.source_session_id::text'
                IN pg_get_functiondef(
                    'require_context_compaction_exact_evidence()'::regprocedure
                )
            ) = 0,
            position(
                'IF NEW.predecessor_compaction_id IS NULL THEN'
                IN pg_get_functiondef(
                    'require_context_compaction_exact_evidence()'::regprocedure
                )
            ) > 0,
            position(
                'WITH RECURSIVE visible_chain AS MATERIALIZED'
                IN pg_get_functiondef(
                    'require_context_compaction_exact_evidence()'::regprocedure
                )
            ) > 0,
            position(
                'context_frontier_preserves_prefix'
                IN pg_get_functiondef(
                    'require_context_compaction_exact_evidence()'::regprocedure
                )
            ) > 0,
            position(
                'context_frontier_member_position'
                IN pg_get_functiondef(
                    'require_context_compaction_exact_evidence()'::regprocedure
                )
            ) > 0",
    )
    .fetch_one(&pool)
    .await?;

    assert_eq!(validator_shape, (true, true, true, true, true, true));

    pool.close().await;
    drop(container);
    Ok(())
}
