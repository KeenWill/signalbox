-- A prior context summary was validated when its immutable compaction and
-- frontier committed. Replaying every prior summary's tool-balance query made
-- the next compaction scan semantic_transcript_entry once per summary. Keep
-- the structural replay that derives model-visible order, validate only the
-- new boundary's tool balance, and address its entries through the typed
-- primary key instead of a concatenated-text scan.
DO $migration$
DECLARE
    definition text;
    updated_definition text;
    prior_summary_balance_check CONSTANT text := $search$
        SELECT
            count(*) FILTER (WHERE entry.payload_kind = 'assistant_tool_use')
            - count(*) FILTER (
                WHERE (entry.payload_kind IN ('tool_execution_result', 'tool_denied', 'tool_closed_by_turn_end') OR (entry.payload_kind = 'delegation_result' AND entry.tool_result_request_id IS NOT NULL))
            )
          INTO mismatch_count
          FROM unnest(visible_entries[visible_first:visible_through])
                   WITH ORDINALITY AS visible(reference, ordinal)
          JOIN semantic_transcript_entry AS entry
            ON visible.reference = entry.source_session_id::text || '/' ||
                entry.semantic_entry_id::text;
        IF mismatch_count <> 0 THEN
            RAISE EXCEPTION 'stored compaction summary leaves a tool exchange open'
                USING ERRCODE = '23514';
        END IF;

$search$;
    untyped_current_boundary_join CONSTANT text := $search$
        ON visible.reference = entry.source_session_id::text || '/' ||
            entry.semantic_entry_id::text;
$search$;
    typed_current_boundary_join CONSTANT text := $replacement$
        ON entry.source_session_id =
               split_part(visible.reference, '/', 1)::uuid
       AND entry.semantic_entry_id =
               split_part(visible.reference, '/', 2)::uuid;
$replacement$;
BEGIN
    SELECT pg_get_functiondef(
        'require_context_compaction_exact_evidence()'::regprocedure
    ) INTO definition;

    updated_definition := replace(
        definition,
        prior_summary_balance_check,
        ''
    );
    IF updated_definition = definition THEN
        RAISE EXCEPTION
            'bounded compaction validation could not remove prior-summary balance replay';
    END IF;
    definition := updated_definition;

    updated_definition := replace(
        definition,
        untyped_current_boundary_join,
        typed_current_boundary_join
    );
    IF updated_definition = definition THEN
        RAISE EXCEPTION
            'bounded compaction validation could not install typed boundary lookup';
    END IF;
    IF strpos(updated_definition, 'entry.source_session_id::text') <> 0 THEN
        RAISE EXCEPTION
            'bounded compaction validation left an untyped entry lookup';
    END IF;

    EXECUTE updated_definition;
END;
$migration$;
