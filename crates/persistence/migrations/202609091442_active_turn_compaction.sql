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

DO $migration$
DECLARE
    definition text;
BEGIN
    SELECT pg_get_functiondef('assert_failed_terminal_execution_without_cancellation(uuid)'::regprocedure)
      INTO definition;
    definition := replace(definition,
        '    ) THEN
        RAISE EXCEPTION
            ''failed tool-loop turn % lacks its exact terminal execution cause''',
        '    ) AND NOT (
        lifecycle.terminal_cause_kind = ''context_compaction_failed''
        AND NOT EXISTS (
            SELECT 1
              FROM compact_session_command AS command
             WHERE command.session_id = lifecycle.session_id
               AND command.automatic_for_turn_id = lifecycle.turn_id
               AND command.result_kind = ''pending''
        )
    ) THEN
        RAISE EXCEPTION
            ''failed tool-loop turn % lacks its exact terminal execution cause''');
    EXECUTE definition;
END;
$migration$;

DO $migration$
DECLARE
    definition text;
BEGIN
    SELECT pg_get_functiondef('assert_tool_loop_turn_final_state_pre_delegation(uuid)'::regprocedure)
      INTO definition;
    definition := replace(definition, $prior$            IF EXISTS (
                SELECT 1 FROM resolve_context_frontier_members(lifecycle.session_id,
                    lifecycle.terminal_frontier_id) AS member
                JOIN runner_placement_boundary AS relocation
                    ON relocation.session_id = member.source_session_id
                    AND relocation.semantic_entry_id = member.semantic_entry_id
                WHERE member.member_position = terminal_member_count - 1
                    AND context_frontier_preserves_prefix(lifecycle.session_id,
                        relocation.context_frontier_id, lifecycle.terminal_frontier_id)
            ) THEN
                terminal_result_boundary_count := terminal_member_count - 1;
            END IF;$prior$, $current$            WHILE EXISTS (
                SELECT 1 FROM resolve_context_frontier_members(lifecycle.session_id,
                    lifecycle.terminal_frontier_id) AS member
                LEFT JOIN runner_placement_boundary AS relocation
                    ON relocation.session_id = member.source_session_id
                    AND relocation.semantic_entry_id = member.semantic_entry_id
                LEFT JOIN context_compaction AS summary
                    ON summary.session_id = member.source_session_id
                    AND summary.summary_entry_id = member.semantic_entry_id
                WHERE member.member_position = terminal_result_boundary_count - 1
                    AND ((relocation.context_frontier_id IS NOT NULL
                        AND context_frontier_preserves_prefix(lifecycle.session_id,
                            relocation.context_frontier_id, lifecycle.terminal_frontier_id))
                        OR (summary.result_frontier_id IS NOT NULL
                            AND context_frontier_preserves_prefix(lifecycle.session_id,
                                summary.result_frontier_id, lifecycle.terminal_frontier_id)))
            ) LOOP
                terminal_result_boundary_count := terminal_result_boundary_count - 1;
            END LOOP;$current$);
    EXECUTE definition;
END;
$migration$;
