-- Keep an already-started turn valid when a later runner relocation advances
-- the session's current placement frontier after the turn's observation.

CREATE OR REPLACE FUNCTION turn_starting_frontier_extends_current_base(
    checked_session_id uuid,
    checked_starting_frontier_id uuid,
    ordinary_base_frontier_id uuid
)
RETURNS boolean LANGUAGE plpgsql STABLE AS $function$
DECLARE
    starting_member_count numeric(20, 0);
    ordinary_member_count numeric(20, 0);
    missing_ordinary_member_count bigint;
    matching_placement_base_count bigint;
BEGIN
    SELECT frontier.member_count
      INTO starting_member_count
      FROM context_frontier AS frontier
     WHERE frontier.owning_session_id = checked_session_id
       AND frontier.context_frontier_id = checked_starting_frontier_id;
    IF starting_member_count IS NULL OR starting_member_count < 1 THEN
        RETURN false;
    END IF;

    IF ordinary_base_frontier_id IS NULL THEN
        ordinary_member_count := 0;
        missing_ordinary_member_count := 0;
    ELSE
        SELECT frontier.member_count
          INTO ordinary_member_count
          FROM context_frontier AS frontier
         WHERE frontier.owning_session_id = checked_session_id
           AND frontier.context_frontier_id = ordinary_base_frontier_id;
        IF ordinary_member_count IS NULL THEN
            RETURN false;
        END IF;
        SELECT count(*)
          INTO missing_ordinary_member_count
          FROM context_frontier_member AS ordinary_member
          LEFT JOIN context_frontier_member AS starting_member
            ON starting_member.owning_session_id = checked_session_id
           AND starting_member.context_frontier_id =
                   checked_starting_frontier_id
           AND starting_member.member_position =
                   ordinary_member.member_position
           AND starting_member.source_session_id =
                   ordinary_member.source_session_id
           AND starting_member.semantic_entry_id =
                   ordinary_member.semantic_entry_id
         WHERE ordinary_member.owning_session_id = checked_session_id
           AND ordinary_member.context_frontier_id = ordinary_base_frontier_id
           AND starting_member.member_position IS NULL;
    END IF;

    IF starting_member_count = ordinary_member_count + 1
       AND missing_ordinary_member_count = 0
    THEN
        RETURN true;
    END IF;

    SELECT count(*)
      INTO matching_placement_base_count
      FROM session_runner_placement_frontier AS pointer
      JOIN context_frontier AS placement_frontier
        ON placement_frontier.owning_session_id = pointer.session_id
       AND placement_frontier.context_frontier_id = pointer.context_frontier_id
       AND placement_frontier.member_count = starting_member_count - 1
     WHERE pointer.session_id = checked_session_id
       AND ordinary_member_count <= placement_frontier.member_count
       AND NOT EXISTS (
            SELECT 1
              FROM context_frontier_member AS placement_member
              LEFT JOIN context_frontier_member AS starting_member
                ON starting_member.owning_session_id = checked_session_id
               AND starting_member.context_frontier_id =
                       checked_starting_frontier_id
               AND starting_member.member_position =
                       placement_member.member_position
               AND starting_member.source_session_id =
                       placement_member.source_session_id
               AND starting_member.semantic_entry_id =
                       placement_member.semantic_entry_id
             WHERE placement_member.owning_session_id = checked_session_id
               AND placement_member.context_frontier_id =
                       placement_frontier.context_frontier_id
               AND starting_member.member_position IS NULL
       )
       AND (
            ordinary_base_frontier_id IS NULL
            OR NOT EXISTS (
                SELECT 1
                  FROM context_frontier_member AS ordinary_member
                  LEFT JOIN context_frontier_member AS placement_member
                    ON placement_member.owning_session_id = checked_session_id
                   AND placement_member.context_frontier_id =
                           placement_frontier.context_frontier_id
                   AND placement_member.member_position =
                           ordinary_member.member_position
                   AND placement_member.source_session_id =
                           ordinary_member.source_session_id
                   AND placement_member.semantic_entry_id =
                           ordinary_member.semantic_entry_id
                 WHERE ordinary_member.owning_session_id = checked_session_id
                   AND ordinary_member.context_frontier_id =
                           ordinary_base_frontier_id
                   AND placement_member.member_position IS NULL
            )
       );

    RETURN matching_placement_base_count = 1;
END;
$function$;

CREATE FUNCTION turn_start_recorded_predecessor_frontier(
    checked_session uuid,
    checked_predecessor_frontier uuid,
    checked_starting_frontier uuid,
    checked_origin_span numeric(20, 0)
)
RETURNS TABLE (
    context_frontier_id uuid,
    member_count numeric(20, 0)
)
LANGUAGE sql STABLE AS $function$
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
           AND NOT EXISTS (
                SELECT 1
                  FROM context_frontier_member AS predecessor_member
                  LEFT JOIN context_frontier_member AS result_member
                    ON result_member.owning_session_id = checked_session
                   AND result_member.context_frontier_id =
                           candidate.result_frontier_id
                   AND result_member.member_position =
                           predecessor_member.member_position
                   AND result_member.source_session_id =
                           predecessor_member.source_session_id
                   AND result_member.semantic_entry_id =
                           predecessor_member.semantic_entry_id
                 WHERE predecessor_member.owning_session_id = checked_session
                   AND predecessor_member.context_frontier_id =
                           checked_predecessor_frontier
                   AND result_member.member_position IS NULL
           )
    ),
    ordinary_base AS (
        SELECT frontier.context_frontier_id, frontier.member_count
          FROM context_frontier AS frontier
         WHERE frontier.owning_session_id = checked_session
           AND frontier.context_frontier_id = COALESCE(
                (SELECT result_frontier_id FROM applicable_leaf),
                checked_predecessor_frontier
           )
    ),
    expected_base AS (
        SELECT frontier.member_count - checked_origin_span AS member_count
          FROM context_frontier AS frontier
         WHERE frontier.owning_session_id = checked_session
           AND frontier.context_frontier_id = checked_starting_frontier
           AND checked_origin_span > 0
           AND frontier.member_count >= checked_origin_span
    ),
    candidate AS (
        SELECT ordinary.context_frontier_id, ordinary.member_count
          FROM ordinary_base AS ordinary
          JOIN expected_base AS expected
            ON expected.member_count = ordinary.member_count
        UNION ALL
        SELECT placement.context_frontier_id, placement.member_count
          FROM ordinary_base AS ordinary
          JOIN session_runner_placement_frontier AS pointer
            ON pointer.session_id = checked_session
          JOIN context_frontier AS placement
            ON placement.owning_session_id = pointer.session_id
           AND placement.context_frontier_id = pointer.context_frontier_id
          JOIN expected_base AS expected
            ON expected.member_count = placement.member_count
         WHERE ordinary.member_count <= placement.member_count
           AND NOT EXISTS (
                SELECT 1
                  FROM context_frontier_member AS ordinary_member
                  LEFT JOIN context_frontier_member AS placement_member
                    ON placement_member.owning_session_id = checked_session
                   AND placement_member.context_frontier_id =
                           placement.context_frontier_id
                   AND placement_member.member_position =
                           ordinary_member.member_position
                   AND placement_member.source_session_id =
                           ordinary_member.source_session_id
                   AND placement_member.semantic_entry_id =
                           ordinary_member.semantic_entry_id
                 WHERE ordinary_member.owning_session_id = checked_session
                   AND ordinary_member.context_frontier_id =
                           ordinary.context_frontier_id
                   AND placement_member.member_position IS NULL
           )
    )
    SELECT candidate.context_frontier_id, candidate.member_count
      FROM candidate
     WHERE NOT EXISTS (
            SELECT 1
              FROM context_frontier_member AS candidate_member
              LEFT JOIN context_frontier_member AS starting_member
                ON starting_member.owning_session_id = checked_session
               AND starting_member.context_frontier_id =
                       checked_starting_frontier
               AND starting_member.member_position =
                       candidate_member.member_position
               AND starting_member.source_session_id =
                       candidate_member.source_session_id
               AND starting_member.semantic_entry_id =
                       candidate_member.semantic_entry_id
             WHERE candidate_member.owning_session_id = checked_session
               AND candidate_member.context_frontier_id =
                       candidate.context_frontier_id
               AND starting_member.member_position IS NULL
     )
     ORDER BY candidate.member_count DESC
     LIMIT 1
$function$;

DO $migration$
DECLARE
    definition text;
    revised text;
BEGIN
    definition := pg_get_functiondef(
        'assert_turn_lifecycle_final_state_without_steering(uuid)'::regprocedure
    );
    revised := replace(
        definition,
        'FROM turn_start_effective_predecessor_frontier(
                   checked_session_id,
                   predecessor_terminal_frontier
               ) AS effective',
        'FROM turn_start_recorded_predecessor_frontier(
                   checked_session_id,
                   predecessor_terminal_frontier,
                   checked_starting_frontier,
                   turn_lifecycle_origin_member_span(
                       checked_turn_id,
                       checked_session_id
                   )
                   + turn_start_model_identity_entry_count(
                       checked_turn_id,
                       checked_starting_frontier
                   )
               ) AS effective'
    );
    IF revised = definition THEN
        RAISE EXCEPTION
            'active-turn recorded runner-placement insertion point is missing';
    END IF;
    EXECUTE revised;

    definition := pg_get_functiondef(
        'assert_terminal_started_turn_common_final_state(uuid)'::regprocedure
    );
    revised := replace(
        definition,
        'FROM turn_start_effective_predecessor_frontier(
                   checked_session,
                   predecessor_frontier
               ) AS effective',
        'FROM turn_start_recorded_predecessor_frontier(
                   checked_session,
                   predecessor_frontier,
                   checked_starting_frontier,
                   turn_lifecycle_origin_member_span(
                       checked_turn_id,
                       checked_session
                   )
                   + turn_start_model_identity_entry_count(
                       checked_turn_id,
                       checked_starting_frontier
                   )
               ) AS effective'
    );
    IF revised = definition THEN
        RAISE EXCEPTION
            'terminal-turn recorded runner-placement insertion point is missing';
    END IF;
    EXECUTE revised;
END;
$migration$;
