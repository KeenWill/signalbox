-- Resolve each recursive frontier once while validating a preserved prefix.
-- Joining the compatibility view directly lets PostgreSQL inline and rerun
-- the recursive resolver once per prefix member on deep session histories.
CREATE FUNCTION context_frontier_preserves_prefix(
    checked_session_id uuid,
    prefix_frontier_id uuid,
    checked_frontier_id uuid
)
RETURNS boolean
LANGUAGE sql
STABLE
AS $$
    WITH prefix_members AS MATERIALIZED (
        SELECT
            member_position,
            source_session_id,
            semantic_entry_id
          FROM context_frontier_member
         WHERE owning_session_id = checked_session_id
           AND context_frontier_id = prefix_frontier_id
    ),
    checked_members AS MATERIALIZED (
        SELECT
            member_position,
            source_session_id,
            semantic_entry_id
          FROM context_frontier_member
         WHERE owning_session_id = checked_session_id
           AND context_frontier_id = checked_frontier_id
    )
    SELECT NOT EXISTS (
        SELECT 1
          FROM prefix_members AS prefix
          LEFT JOIN checked_members AS checked
            ON checked.member_position = prefix.member_position
         WHERE ROW(
                checked.source_session_id,
                checked.semantic_entry_id
           ) IS DISTINCT FROM ROW(
                prefix.source_session_id,
                prefix.semantic_entry_id
           )
    )
$$;

DO $migration$
DECLARE
    workflow_schema name := pg_catalog.current_schema();
BEGIN
    IF workflow_schema IS NULL THEN
        RAISE EXCEPTION
            'frontier prefix validation migration requires a current schema';
    END IF;
    EXECUTE pg_catalog.format(
        'ALTER FUNCTION %I.context_frontier_preserves_prefix(uuid,uuid,uuid) '
        'SET search_path TO %I, pg_catalog, pg_temp',
        workflow_schema,
        workflow_schema
    );
END;
$migration$;

DO $migration$
DECLARE
    definition text;
    updated_definition text;
    starting_prefix_check CONSTANT text := $search$
       OR EXISTS (
            SELECT 1
              FROM context_frontier_member AS starting
              LEFT JOIN context_frontier_member AS checked
                ON checked.owning_session_id = starting.owning_session_id
               AND checked.context_frontier_id = checked_frontier
               AND checked.member_position = starting.member_position
             WHERE starting.owning_session_id = checked_session
               AND starting.context_frontier_id = starting_frontier
               AND ROW(
                    checked.source_session_id,
                    checked.semantic_entry_id
               ) IS DISTINCT FROM ROW(
                    starting.source_session_id,
                    starting.semantic_entry_id
               )
       )
$search$;
    materialized_starting_prefix_check CONSTANT text := $replacement$
       OR NOT context_frontier_preserves_prefix(
            checked_session,
            starting_frontier,
            checked_frontier
       )
$replacement$;
    boundary_prefix_check CONSTANT text := $search$
           OR EXISTS (
                SELECT 1
                  FROM context_frontier_member AS boundary
                  LEFT JOIN context_frontier_member AS checked
                    ON checked.owning_session_id =
                       boundary.owning_session_id
                   AND checked.context_frontier_id = checked_frontier
                   AND checked.member_position = boundary.member_position
                 WHERE boundary.owning_session_id = checked_session
                   AND boundary.context_frontier_id = result_boundary
                   AND ROW(
                        checked.source_session_id,
                        checked.semantic_entry_id
                   ) IS DISTINCT FROM ROW(
                        boundary.source_session_id,
                        boundary.semantic_entry_id
                   )
           )
$search$;
    materialized_boundary_prefix_check CONSTANT text := $replacement$
           OR NOT context_frontier_preserves_prefix(
                checked_session,
                result_boundary,
                checked_frontier
           )
$replacement$;
BEGIN
    SELECT pg_get_functiondef(
        'assert_model_call_steering_final_state(uuid)'::regprocedure
    ) INTO definition;

    updated_definition := replace(
        definition,
        starting_prefix_check,
        materialized_starting_prefix_check
    );
    IF updated_definition = definition THEN
        RAISE EXCEPTION
            'frontier materialization could not replace the turn-start prefix check';
    END IF;
    definition := updated_definition;

    updated_definition := replace(
        definition,
        boundary_prefix_check,
        materialized_boundary_prefix_check
    );
    IF updated_definition = definition THEN
        RAISE EXCEPTION
            'frontier materialization could not replace the tool-round prefix check';
    END IF;
    EXECUTE updated_definition;
END;
$migration$;

DO $migration$
DECLARE
    definition text;
    updated_definition text;
    boundary_prefix_check CONSTANT text := $search$
           AND NOT EXISTS (
                SELECT 1
                  FROM context_frontier_member AS boundary_member
                  LEFT JOIN context_frontier_member AS checked_member
                    ON checked_member.owning_session_id =
                       boundary_member.owning_session_id
                   AND checked_member.context_frontier_id =
                       checked_frontier_id
                   AND checked_member.member_position =
                       boundary_member.member_position
                   AND checked_member.source_session_id =
                       boundary_member.source_session_id
                   AND checked_member.semantic_entry_id =
                       boundary_member.semantic_entry_id
                 WHERE boundary_member.owning_session_id =
                       predecessor_round.session_id
                   AND boundary_member.context_frontier_id =
                       predecessor_round.boundary_frontier_id
                   AND checked_member.member_position IS NULL
           )
$search$;
    materialized_boundary_prefix_check CONSTANT text := $replacement$
           AND context_frontier_preserves_prefix(
                predecessor_round.session_id,
                predecessor_round.boundary_frontier_id,
                checked_frontier_id
           )
$replacement$;
BEGIN
    SELECT pg_get_functiondef(
        'continuation_frontier_closes_predecessor_tool_round(uuid,uuid,uuid,uuid)'::regprocedure
    ) INTO definition;

    updated_definition := replace(
        definition,
        boundary_prefix_check,
        materialized_boundary_prefix_check
    );
    IF updated_definition = definition THEN
        RAISE EXCEPTION
            'frontier materialization could not replace the continuation prefix check';
    END IF;
    EXECUTE updated_definition;
END;
$migration$;
