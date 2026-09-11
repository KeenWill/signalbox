DO $migration$
DECLARE
    definition text;
    prior text := $prior$            WHILE EXISTS (
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
            END LOOP;$prior$;
BEGIN
    SELECT pg_get_functiondef('assert_tool_loop_turn_final_state_pre_delegation(uuid)'::regprocedure)
      INTO definition;
    IF strpos(definition, prior) = 0 THEN
        RAISE EXCEPTION 'terminal tool boundary scan is missing';
    END IF;
    EXECUTE replace(definition, prior, $current$            WITH RECURSIVE terminal_members AS MATERIALIZED (
                SELECT * FROM resolve_context_frontier_members(
                    lifecycle.session_id, lifecycle.terminal_frontier_id)
            ),
            boundaries AS MATERIALIZED (
                SELECT member.member_position,
                       relocation.context_frontier_id AS relocation_frontier,
                       summary.result_frontier_id AS summary_frontier
                  FROM terminal_members AS member
                  LEFT JOIN runner_placement_boundary AS relocation
                    ON relocation.session_id = member.source_session_id
                   AND relocation.semantic_entry_id = member.semantic_entry_id
                  LEFT JOIN context_compaction AS summary
                    ON summary.session_id = member.source_session_id
                   AND summary.summary_entry_id = member.semantic_entry_id
                 WHERE member.member_position < terminal_member_count
            ),
            boundary_chain AS MATERIALIZED (
                SELECT frontier.context_frontier_id, frontier.prefix_context_frontier_id
                  FROM context_frontier AS frontier
                 WHERE frontier.owning_session_id = lifecycle.session_id
                   AND frontier.context_frontier_id IN (
                       SELECT relocation_frontier FROM boundaries
                       UNION SELECT summary_frontier FROM boundaries
                   )
                UNION
                SELECT prefix.context_frontier_id, prefix.prefix_context_frontier_id
                  FROM boundary_chain AS chain
                  JOIN context_frontier AS prefix
                    ON prefix.owning_session_id = lifecycle.session_id
                   AND prefix.context_frontier_id = chain.prefix_context_frontier_id
            ),
            mismatched_frontiers AS (
                SELECT DISTINCT chain.context_frontier_id
                  FROM boundary_chain AS chain
                  JOIN context_frontier_delta AS delta
                    ON delta.owning_session_id = lifecycle.session_id
                   AND delta.context_frontier_id = chain.context_frontier_id
                  LEFT JOIN terminal_members AS terminal
                    ON terminal.member_position = delta.member_position
                 WHERE ROW(terminal.source_session_id, terminal.semantic_entry_id)
                       IS DISTINCT FROM ROW(delta.source_session_id, delta.semantic_entry_id)
                UNION
                SELECT child.context_frontier_id
                  FROM mismatched_frontiers AS mismatch
                  JOIN boundary_chain AS child
                    ON child.prefix_context_frontier_id = mismatch.context_frontier_id
            )
            SELECT COALESCE(max(member_position), 0) + 1
              INTO terminal_result_boundary_count
              FROM boundaries
             WHERE NOT (
                 (relocation_frontier IS NOT NULL AND relocation_frontier NOT IN (
                     SELECT context_frontier_id FROM mismatched_frontiers
                 ))
                 OR (summary_frontier IS NOT NULL AND summary_frontier NOT IN (
                     SELECT context_frontier_id FROM mismatched_frontiers
                 ))
             );$current$);
END;
$migration$;
