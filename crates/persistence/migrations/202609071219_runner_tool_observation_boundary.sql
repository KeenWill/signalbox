DO $migration$
DECLARE definition text;
BEGIN
    SELECT pg_get_functiondef('assert_tool_round_final_state(uuid)'::regprocedure) INTO definition;
    definition := replace(definition,
        'IS DISTINCT FROM source_count + round_record.response_part_count
        THEN',
        'IS DISTINCT FROM source_count + round_record.response_part_count
           AND NOT EXISTS (
                SELECT 1 FROM runner_placement_boundary AS relocation
                JOIN context_frontier AS boundary
                  ON boundary.owning_session_id = relocation.session_id
                 AND boundary.context_frontier_id = relocation.context_frontier_id
                WHERE relocation.session_id = round_record.session_id
                  AND relocation.context_frontier_id = round_record.boundary_frontier_id
                  AND boundary_count = source_count + round_record.response_part_count + 1
                  AND (SELECT count(*) FROM resolve_context_frontier_members(
                        relocation.session_id, boundary.prefix_context_frontier_id))
                      = source_count + round_record.response_part_count
           )
        THEN');
    EXECUTE definition;

    SELECT pg_get_functiondef('require_runner_placement_boundary()'::regprocedure) INTO definition;
    definition := replace(definition,
        '    THEN
        RAISE EXCEPTION ''placement boundary precedes complete batch results''',
        '       AND NOT EXISTS (
            SELECT 1 FROM tool_round AS observed
            JOIN model_call AS call
              ON call.model_call_id = observed.producing_model_call_id
             AND call.session_id = observed.session_id AND call.turn_id = observed.turn_id
            WHERE observed.session_id = NEW.session_id
              AND observed.boundary_kind = ''continuing''
              AND observed.boundary_frontier_id = NEW.context_frontier_id
              AND call.state_kind = ''terminal'' AND call.terminal_disposition_kind = ''completed''
       )
    THEN
        RAISE EXCEPTION ''placement boundary precedes complete batch results''');
    definition := replace(definition,
        '        UNION SELECT seed_context_frontier_id FROM imported_session_seed WHERE session_id = NEW.session_id',
        '        UNION SELECT boundary.prefix_context_frontier_id
            FROM tool_round AS observed
            JOIN context_frontier AS boundary
              ON boundary.owning_session_id = observed.session_id
             AND boundary.context_frontier_id = observed.boundary_frontier_id
            WHERE observed.session_id = NEW.session_id
              AND observed.boundary_kind = ''continuing''
              AND observed.boundary_frontier_id = NEW.context_frontier_id
        UNION SELECT seed_context_frontier_id FROM imported_session_seed WHERE session_id = NEW.session_id');
    EXECUTE definition;
END;
$migration$;
