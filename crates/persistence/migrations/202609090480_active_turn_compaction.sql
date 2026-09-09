ALTER TABLE turn_lifecycle
    ADD COLUMN compaction_frontier_id uuid,
    ADD FOREIGN KEY (session_id, compaction_frontier_id)
        REFERENCES context_frontier(owning_session_id, context_frontier_id);

DO $migration$
DECLARE
    definition text;
BEGIN
    SELECT pg_get_functiondef('assert_model_call_steering_final_state(uuid)'::regprocedure)
      INTO definition;
    definition := replace(definition,
        '    SELECT count(*)
      INTO suffix_count',
        '    WHILE EXISTS (
        SELECT 1 FROM context_frontier_member AS member
        LEFT JOIN context_compaction AS summary
          ON summary.session_id = member.source_session_id
         AND summary.summary_entry_id = member.semantic_entry_id
        LEFT JOIN runner_placement_boundary AS relocation
          ON relocation.session_id = member.source_session_id
         AND relocation.semantic_entry_id = member.semantic_entry_id
        WHERE member.owning_session_id = checked_session
          AND member.context_frontier_id = checked_frontier
          AND member.member_position = suffix_start_count + 1
          AND ((summary.result_frontier_id IS NOT NULL
              AND context_frontier_preserves_prefix(checked_session,
                  summary.result_frontier_id, checked_frontier))
              OR (relocation.context_frontier_id IS NOT NULL
                  AND context_frontier_preserves_prefix(checked_session,
                      relocation.context_frontier_id, checked_frontier)))
    ) LOOP
        suffix_start_count := suffix_start_count + 1;
    END LOOP;

    SELECT count(*)
      INTO suffix_count');
    EXECUTE definition;
END;
$migration$;
