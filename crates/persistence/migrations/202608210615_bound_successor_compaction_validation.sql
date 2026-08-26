-- Locate one immutable frontier member without expanding the compatibility
-- view's complete ancestry. Each recursive step stops as soon as the member is
-- present in that frontier's delta.
CREATE FUNCTION context_frontier_member_position(
    checked_session_id uuid,
    checked_frontier_id uuid,
    checked_source_session_id uuid,
    checked_semantic_entry_id uuid
)
RETURNS numeric(20, 0)
LANGUAGE sql
STABLE
AS $$
    WITH RECURSIVE checked_chain AS MATERIALIZED (
        SELECT
            frontier.context_frontier_id,
            frontier.prefix_context_frontier_id
          FROM context_frontier AS frontier
         WHERE frontier.owning_session_id = checked_session_id
           AND frontier.context_frontier_id = checked_frontier_id
        UNION ALL
        SELECT
            prefix.context_frontier_id,
            prefix.prefix_context_frontier_id
          FROM checked_chain AS chain
          JOIN context_frontier AS prefix
            ON prefix.owning_session_id = checked_session_id
           AND prefix.context_frontier_id =
                   chain.prefix_context_frontier_id
         WHERE NOT EXISTS (
                SELECT 1
                  FROM context_frontier_delta AS found
                 WHERE found.owning_session_id = checked_session_id
                   AND found.context_frontier_id = chain.context_frontier_id
                   AND found.source_session_id = checked_source_session_id
                   AND found.semantic_entry_id = checked_semantic_entry_id
         )
    )
    SELECT member.member_position
      FROM checked_chain AS chain
      JOIN context_frontier_delta AS member
        ON member.owning_session_id = checked_session_id
       AND member.context_frontier_id = chain.context_frontier_id
       AND member.source_session_id = checked_source_session_id
       AND member.semantic_entry_id = checked_semantic_entry_id
     LIMIT 1
$$;

-- Stop the ancestry proof once it reaches the requested prefix. The exact
-- member fallback still reaches the root for independently built frontiers.
CREATE OR REPLACE FUNCTION context_frontier_preserves_prefix(
    checked_session_id uuid,
    prefix_frontier_id uuid,
    checked_frontier_id uuid
)
RETURNS boolean
LANGUAGE sql
STABLE
AS $$
    WITH RECURSIVE checked_chain AS MATERIALIZED (
        SELECT
            frontier.context_frontier_id,
            frontier.prefix_context_frontier_id
          FROM context_frontier AS frontier
         WHERE frontier.owning_session_id = checked_session_id
           AND frontier.context_frontier_id = checked_frontier_id
        UNION
        SELECT
            prefix.context_frontier_id,
            prefix.prefix_context_frontier_id
          FROM checked_chain AS chain
          JOIN context_frontier AS prefix
            ON prefix.owning_session_id = checked_session_id
           AND prefix.context_frontier_id = chain.prefix_context_frontier_id
         WHERE chain.context_frontier_id <> prefix_frontier_id
    ),
    prefix_members AS MATERIALIZED (
        SELECT member_position, source_session_id, semantic_entry_id
          FROM context_frontier_member
         WHERE owning_session_id = checked_session_id
           AND context_frontier_id = prefix_frontier_id
    ),
    checked_members AS MATERIALIZED (
        SELECT member_position, source_session_id, semantic_entry_id
          FROM context_frontier_member
         WHERE owning_session_id = checked_session_id
           AND context_frontier_id = checked_frontier_id
    )
    SELECT CASE
        WHEN EXISTS (
            SELECT 1 FROM checked_chain
             WHERE context_frontier_id = prefix_frontier_id
        ) THEN true
        ELSE NOT EXISTS (
            SELECT 1
              FROM prefix_members AS prefix
              LEFT JOIN checked_members AS checked
                ON checked.member_position = prefix.member_position
             WHERE ROW(checked.source_session_id, checked.semantic_entry_id)
                   IS DISTINCT FROM
                   ROW(prefix.source_session_id, prefix.semantic_entry_id)
        )
    END
$$;

-- A predecessor compaction already proved every earlier summary and its
-- model-visible boundary. Validate a successor from that immutable checkpoint
-- and only the retained/current suffix. Root compactions retain the complete
-- replay required for imported, independently rooted frontiers.
CREATE OR REPLACE FUNCTION require_context_compaction_exact_evidence()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    source_count numeric(20, 0);
    result_count numeric(20, 0);
    through_position numeric(20, 0);
    predecessor_result uuid;
    predecessor_summary_entry uuid;
    consumed_through_source_session uuid;
    consumed_through_entry uuid;
    predecessor_consumed_position numeric(20, 0);
    mismatch_count bigint;
    visible_entries text[];
    visible_first integer;
    visible_through integer;
    visible_summary integer;
    summary_record record;
    summary_reference text;
    visible_suffix text[];
BEGIN
    SELECT count(*)
      INTO mismatch_count
      FROM context_compaction_model_call AS call
     WHERE call.model_call_id = NEW.producing_call_id
       AND call.session_id = NEW.session_id
       AND call.source_frontier_id = NEW.source_frontier_id
       AND call.state_kind = 'terminal'
       AND call.terminal_disposition_kind = 'completed';
    IF mismatch_count <> 1 THEN
        RAISE EXCEPTION 'compaction requires its exact completed dedicated call'
            USING ERRCODE = '23514';
    END IF;

    SELECT count(*)
      INTO mismatch_count
      FROM semantic_transcript_entry AS summary
     WHERE summary.source_session_id = NEW.session_id
       AND summary.semantic_entry_id = NEW.summary_entry_id
       AND summary.payload_kind = 'context_summary'
       AND summary.context_summary_producing_call_id = NEW.producing_call_id
       AND summary.context_summary_first_source_session_id = NEW.first_source_session_id
       AND summary.context_summary_first_entry_id = NEW.first_entry_id
       AND summary.context_summary_through_source_session_id = NEW.through_source_session_id
       AND summary.context_summary_through_entry_id = NEW.through_entry_id;
    IF mismatch_count <> 1 THEN
        RAISE EXCEPTION 'compaction requires its exact summary provenance'
            USING ERRCODE = '23514';
    END IF;

    SELECT member_count INTO source_count
      FROM context_frontier
     WHERE owning_session_id = NEW.session_id
       AND context_frontier_id = NEW.source_frontier_id;
    SELECT member_count INTO result_count
      FROM context_frontier
     WHERE owning_session_id = NEW.session_id
       AND context_frontier_id = NEW.result_frontier_id;
    through_position := context_frontier_member_position(
        NEW.session_id,
        NEW.source_frontier_id,
        NEW.through_source_session_id,
        NEW.through_entry_id
    );
    IF source_count IS NULL
       OR result_count IS NULL
       OR through_position IS NULL
       OR result_count <> source_count + 1
    THEN
        RAISE EXCEPTION 'compaction range or frontier cardinality is invalid'
            USING ERRCODE = '23514';
    END IF;

    IF NEW.predecessor_compaction_id IS NULL THEN
        SELECT array_agg(
                   member.source_session_id::text || '/' ||
                   member.semantic_entry_id::text
                   ORDER BY member.member_position
               )
          INTO visible_entries
          FROM context_frontier_member AS member
         WHERE member.owning_session_id = NEW.session_id
           AND member.context_frontier_id = NEW.source_frontier_id;

        FOR summary_record IN
            SELECT
                member.source_session_id,
                member.semantic_entry_id,
                entry.context_summary_first_source_session_id AS first_session,
                entry.context_summary_first_entry_id AS first_entry,
                entry.context_summary_through_source_session_id AS through_session,
                entry.context_summary_through_entry_id AS through_entry
              FROM context_frontier_member AS member
              JOIN semantic_transcript_entry AS entry
                ON entry.source_session_id = member.source_session_id
               AND entry.semantic_entry_id = member.semantic_entry_id
             WHERE member.owning_session_id = NEW.session_id
               AND member.context_frontier_id = NEW.source_frontier_id
               AND entry.payload_kind = 'context_summary'
             ORDER BY member.member_position
        LOOP
            summary_reference := summary_record.source_session_id::text || '/' ||
                summary_record.semantic_entry_id::text;
            visible_first := array_position(
                visible_entries,
                summary_record.first_session::text || '/' || summary_record.first_entry::text
            );
            visible_through := array_position(
                visible_entries,
                summary_record.through_session::text || '/' || summary_record.through_entry::text
            );
            visible_summary := array_position(visible_entries, summary_reference);
            IF visible_first IS NULL
               OR visible_first <> 1
               OR visible_through IS NULL
               OR visible_summary IS NULL
               OR visible_summary <= visible_through
            THEN
                RAISE EXCEPTION 'stored compaction summary has an invalid visible range'
                    USING ERRCODE = '23514';
            END IF;

            SELECT array_agg(visible.reference ORDER BY visible.ordinal)
              INTO visible_suffix
              FROM unnest(
                       visible_entries[
                           visible_through + 1:array_length(visible_entries, 1)
                       ]
                   ) WITH ORDINALITY AS visible(reference, ordinal)
             WHERE visible.reference <> summary_reference;
            visible_entries := ARRAY[summary_reference] ||
                COALESCE(visible_suffix, ARRAY[]::text[]);
        END LOOP;

        visible_first := array_position(
            visible_entries,
            NEW.first_source_session_id::text || '/' || NEW.first_entry_id::text
        );
        visible_through := array_position(
            visible_entries,
            NEW.through_source_session_id::text || '/' || NEW.through_entry_id::text
        );
        IF visible_first IS NULL OR visible_first <> 1 OR visible_through IS NULL THEN
            RAISE EXCEPTION 'compaction range must start at the visible frontier start'
                USING ERRCODE = '23514';
        END IF;

        SELECT
            count(*) FILTER (WHERE entry.payload_kind = 'assistant_tool_use')
            - count(*) FILTER (
                WHERE entry.payload_kind IN (
                    'tool_execution_result', 'tool_denied', 'tool_closed_by_turn_end'
                )
                   OR (
                        entry.payload_kind = 'delegation_result'
                        AND entry.tool_result_request_id IS NOT NULL
                   )
            )
          INTO mismatch_count
          FROM unnest(visible_entries[visible_first:visible_through])
                   WITH ORDINALITY AS visible(reference, ordinal)
          JOIN semantic_transcript_entry AS entry
            ON entry.source_session_id = split_part(visible.reference, '/', 1)::uuid
           AND entry.semantic_entry_id = split_part(visible.reference, '/', 2)::uuid;
    ELSE
        SELECT
            predecessor.result_frontier_id,
            predecessor.summary_entry_id
          INTO
            predecessor_result,
            predecessor_summary_entry
          FROM context_compaction AS predecessor
         WHERE predecessor.context_compaction_id = NEW.predecessor_compaction_id
           AND predecessor.session_id = NEW.session_id;
        WITH RECURSIVE predecessor_chain AS MATERIALIZED (
            SELECT
                predecessor.context_compaction_id,
                predecessor.predecessor_compaction_id,
                predecessor.through_source_session_id,
                predecessor.through_entry_id,
                0 AS depth
              FROM context_compaction AS predecessor
             WHERE predecessor.context_compaction_id =
                       NEW.predecessor_compaction_id
               AND predecessor.session_id = NEW.session_id
            UNION ALL
            SELECT
                ancestor.context_compaction_id,
                ancestor.predecessor_compaction_id,
                ancestor.through_source_session_id,
                ancestor.through_entry_id,
                chain.depth + 1
              FROM predecessor_chain AS chain
              JOIN context_compaction AS ancestor
                ON ancestor.context_compaction_id =
                       chain.predecessor_compaction_id
               AND ancestor.session_id = NEW.session_id
        )
        SELECT
            chain.through_source_session_id,
            chain.through_entry_id
          INTO consumed_through_source_session, consumed_through_entry
          FROM predecessor_chain AS chain
         WHERE NOT EXISTS (
                SELECT 1
                  FROM context_compaction AS consumed_summary
                 WHERE consumed_summary.session_id = NEW.session_id
                   AND consumed_summary.summary_entry_id =
                           chain.through_entry_id
                   AND chain.through_source_session_id = NEW.session_id
         )
         ORDER BY chain.depth
         LIMIT 1;
        predecessor_consumed_position := context_frontier_member_position(
            NEW.session_id,
            NEW.source_frontier_id,
            consumed_through_source_session,
            consumed_through_entry
        );
        IF predecessor_result IS NULL
           OR predecessor_consumed_position IS NULL
           OR NOT context_frontier_preserves_prefix(
                NEW.session_id,
                predecessor_result,
                NEW.source_frontier_id
           )
           OR NEW.first_source_session_id <> NEW.session_id
           OR NEW.first_entry_id <> predecessor_summary_entry
        THEN
            RAISE EXCEPTION 'compaction predecessor result and visible start must match'
                USING ERRCODE = '23514';
        END IF;
        IF ROW(NEW.through_source_session_id, NEW.through_entry_id) <>
               ROW(NEW.session_id, predecessor_summary_entry)
           AND (
                through_position <= predecessor_consumed_position
                OR EXISTS (
                    SELECT 1
                      FROM context_compaction AS consumed_summary
                     WHERE consumed_summary.session_id = NEW.session_id
                       AND consumed_summary.summary_entry_id = NEW.through_entry_id
                       AND NEW.through_source_session_id = NEW.session_id
                )
           )
        THEN
            RAISE EXCEPTION 'compaction range must start at the visible frontier start'
                USING ERRCODE = '23514';
        END IF;

        WITH RECURSIVE visible_chain AS MATERIALIZED (
            SELECT
                frontier.context_frontier_id,
                frontier.prefix_context_frontier_id,
                frontier.member_count
              FROM context_frontier AS frontier
             WHERE frontier.owning_session_id = NEW.session_id
               AND frontier.context_frontier_id = NEW.source_frontier_id
            UNION ALL
            SELECT
                prefix.context_frontier_id,
                prefix.prefix_context_frontier_id,
                prefix.member_count
              FROM visible_chain AS chain
              JOIN context_frontier AS prefix
                ON prefix.owning_session_id = NEW.session_id
               AND prefix.context_frontier_id = chain.prefix_context_frontier_id
             WHERE chain.member_count > predecessor_consumed_position
        ),
        current_boundary_entries AS MATERIALIZED (
            SELECT member.source_session_id, member.semantic_entry_id
              FROM visible_chain AS chain
             JOIN context_frontier_delta AS member
                ON member.owning_session_id = NEW.session_id
               AND member.context_frontier_id = chain.context_frontier_id
             WHERE ROW(NEW.through_source_session_id, NEW.through_entry_id) <>
                       ROW(NEW.session_id, predecessor_summary_entry)
               AND member.member_position > predecessor_consumed_position
               AND member.member_position <= through_position
               AND NOT EXISTS (
                    SELECT 1
                      FROM context_compaction AS consumed_summary
                     WHERE consumed_summary.session_id = NEW.session_id
                       AND consumed_summary.summary_entry_id = member.semantic_entry_id
                       AND member.source_session_id = NEW.session_id
               )
            UNION ALL
            SELECT NEW.session_id, predecessor_summary_entry
        )
        SELECT
            count(*) FILTER (WHERE entry.payload_kind = 'assistant_tool_use')
            - count(*) FILTER (
                WHERE entry.payload_kind IN (
                    'tool_execution_result', 'tool_denied', 'tool_closed_by_turn_end'
                )
                   OR (
                        entry.payload_kind = 'delegation_result'
                        AND entry.tool_result_request_id IS NOT NULL
                   )
            )
          INTO mismatch_count
          FROM current_boundary_entries AS visible
          JOIN semantic_transcript_entry AS entry
            ON entry.source_session_id = visible.source_session_id
           AND entry.semantic_entry_id = visible.semantic_entry_id;
    END IF;

    IF mismatch_count <> 0 THEN
        RAISE EXCEPTION 'compaction boundary leaves a tool exchange open'
            USING ERRCODE = '23514';
    END IF;

    IF NOT context_frontier_preserves_prefix(
            NEW.session_id,
            NEW.source_frontier_id,
            NEW.result_frontier_id
       )
       OR context_frontier_member_position(
            NEW.session_id,
            NEW.result_frontier_id,
            NEW.session_id,
            NEW.summary_entry_id
       ) <> result_count
    THEN
        RAISE EXCEPTION 'compaction result must be the source plus its summary'
            USING ERRCODE = '23514';
    END IF;

    RETURN NULL;
END;
$$;

DO $migration$
DECLARE
    workflow_schema name := pg_catalog.current_schema();
BEGIN
    IF workflow_schema IS NULL THEN
        RAISE EXCEPTION
            'bounded successor compaction migration requires a current schema';
    END IF;
    EXECUTE pg_catalog.format(
        'ALTER FUNCTION %I.context_frontier_member_position(uuid,uuid,uuid,uuid) '
        'SET search_path TO %I, pg_catalog, pg_temp',
        workflow_schema,
        workflow_schema
    );
    EXECUTE pg_catalog.format(
        'ALTER FUNCTION %I.context_frontier_preserves_prefix(uuid,uuid,uuid) '
        'SET search_path TO %I, pg_catalog, pg_temp',
        workflow_schema,
        workflow_schema
    );
    EXECUTE pg_catalog.format(
        'ALTER FUNCTION %I.require_context_compaction_exact_evidence() '
        'SET search_path TO %I, pg_catalog, pg_temp',
        workflow_schema,
        workflow_schema
    );
END;
$migration$;
