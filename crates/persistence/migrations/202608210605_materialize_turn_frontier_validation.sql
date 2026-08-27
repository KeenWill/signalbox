-- Lifecycle transitions compare several recursive frontier memberships while
-- holding the session and outbox locks. Reuse the materialized prefix helper
-- so each comparison resolves each immutable chain only once.
CREATE OR REPLACE FUNCTION turn_start_effective_predecessor_frontier(
    checked_session uuid,
    checked_predecessor_frontier uuid
)
RETURNS TABLE (
    context_frontier_id uuid,
    member_count numeric(20, 0)
)
LANGUAGE sql
STABLE
AS $$
    WITH applicable_leaf AS (
        SELECT candidate.result_frontier_id
          FROM context_compaction AS candidate
         WHERE candidate.session_id = checked_session
           AND NOT EXISTS (
                SELECT 1
                  FROM context_compaction AS successor
                 WHERE successor.session_id = candidate.session_id
                   AND successor.predecessor_compaction_id =
                           candidate.context_compaction_id
           )
           AND context_frontier_preserves_prefix(
                checked_session,
                checked_predecessor_frontier,
                candidate.result_frontier_id
           )
    )
    SELECT frontier.context_frontier_id, frontier.member_count
      FROM context_frontier AS frontier
     WHERE frontier.owning_session_id = checked_session
       AND frontier.context_frontier_id = COALESCE(
            (SELECT result_frontier_id FROM applicable_leaf),
            checked_predecessor_frontier
       )
$$;

DO $migration$
DECLARE
    definition text;
    updated_definition text;
    predecessor_prefix_check CONSTANT text := $search$
        SELECT count(*)
          INTO prefix_mismatch_count
          FROM context_frontier_member AS predecessor_member
          LEFT JOIN context_frontier_member AS starting_member
            ON starting_member.owning_session_id = checked_session
           AND starting_member.context_frontier_id = checked_starting_frontier
           AND starting_member.member_position = predecessor_member.member_position
           AND starting_member.source_session_id = predecessor_member.source_session_id
           AND starting_member.semantic_entry_id = predecessor_member.semantic_entry_id
         WHERE predecessor_member.owning_session_id = checked_session
           AND predecessor_member.context_frontier_id = predecessor_frontier
           AND starting_member.member_position IS NULL;
$search$;
    materialized_predecessor_prefix_check CONSTANT text := $replacement$
        SELECT CASE
                   WHEN context_frontier_preserves_prefix(
                        checked_session,
                        predecessor_frontier,
                        checked_starting_frontier
                   ) THEN 0
                   ELSE 1
               END
          INTO prefix_mismatch_count;
$replacement$;
BEGIN
    SELECT pg_get_functiondef(
        'assert_terminal_started_turn_common_final_state(uuid)'::regprocedure
    ) INTO definition;
    updated_definition := replace(
        definition,
        predecessor_prefix_check,
        materialized_predecessor_prefix_check
    );
    IF updated_definition = definition THEN
        RAISE EXCEPTION
            'turn frontier materialization could not replace the terminal predecessor prefix check';
    END IF;
    EXECUTE updated_definition;
END;
$migration$;

DO $migration$
DECLARE
    definition text;
    updated_definition text;
    predecessor_prefix_check CONSTANT text := $search$
        SELECT count(*)
          INTO prefix_mismatch_count
          FROM context_frontier_member AS predecessor_member
          LEFT JOIN context_frontier_member AS starting_member
            ON starting_member.owning_session_id = checked_session_id
           AND starting_member.context_frontier_id = checked_starting_frontier
           AND starting_member.member_position = predecessor_member.member_position
           AND starting_member.source_session_id = predecessor_member.source_session_id
           AND starting_member.semantic_entry_id = predecessor_member.semantic_entry_id
         WHERE predecessor_member.owning_session_id = checked_session_id
           AND predecessor_member.context_frontier_id = predecessor_terminal_frontier
           AND starting_member.member_position IS NULL;
$search$;
    materialized_predecessor_prefix_check CONSTANT text := $replacement$
        SELECT CASE
                   WHEN context_frontier_preserves_prefix(
                        checked_session_id,
                        predecessor_terminal_frontier,
                        checked_starting_frontier
                   ) THEN 0
                   ELSE 1
               END
          INTO prefix_mismatch_count;
$replacement$;
    terminal_prefix_check CONSTANT text := $search$
    SELECT count(*)
      INTO prefix_mismatch_count
      FROM context_frontier_member AS starting_member
      LEFT JOIN context_frontier_member AS terminal_member
        ON terminal_member.owning_session_id = checked_session_id
       AND terminal_member.context_frontier_id = checked_terminal_frontier
       AND terminal_member.member_position = starting_member.member_position
       AND terminal_member.source_session_id = starting_member.source_session_id
       AND terminal_member.semantic_entry_id = starting_member.semantic_entry_id
     WHERE starting_member.owning_session_id = checked_session_id
       AND starting_member.context_frontier_id = checked_starting_frontier
       AND terminal_member.member_position IS NULL;
$search$;
    materialized_terminal_prefix_check CONSTANT text := $replacement$
    SELECT CASE
               WHEN context_frontier_preserves_prefix(
                    checked_session_id,
                    checked_starting_frontier,
                    checked_terminal_frontier
               ) THEN 0
               ELSE 1
           END
      INTO prefix_mismatch_count;
$replacement$;
BEGIN
    SELECT pg_get_functiondef(
        'assert_turn_lifecycle_final_state_without_steering(uuid)'::regprocedure
    ) INTO definition;
    updated_definition := replace(
        definition,
        predecessor_prefix_check,
        materialized_predecessor_prefix_check
    );
    IF updated_definition = definition THEN
        RAISE EXCEPTION
            'turn frontier materialization could not replace the active predecessor prefix check';
    END IF;
    definition := updated_definition;
    updated_definition := replace(
        definition,
        terminal_prefix_check,
        materialized_terminal_prefix_check
    );
    IF updated_definition = definition THEN
        RAISE EXCEPTION
            'turn frontier materialization could not replace the terminal frontier prefix check';
    END IF;
    EXECUTE updated_definition;
END;
$migration$;
