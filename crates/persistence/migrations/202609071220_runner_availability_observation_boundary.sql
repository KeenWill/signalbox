DO $migration$
DECLARE definition text;
BEGIN
    SELECT pg_get_functiondef('require_runner_placement_boundary()'::regprocedure) INTO definition;
    definition := replace(definition,
        '        UNION SELECT seed_context_frontier_id FROM imported_session_seed WHERE session_id = NEW.session_id',
        '        UNION SELECT call.context_frontier_id
            FROM credential_pool_availability_successor AS successor
            JOIN model_call AS call ON call.model_call_id = successor.predecessor_model_call_id
            JOIN turn_lifecycle AS turn ON turn.session_id = call.session_id
                AND turn.turn_id = call.turn_id
                AND turn.current_attempt_id = successor.successor_turn_attempt_id
            JOIN context_frontier AS boundary ON boundary.owning_session_id = call.session_id
                AND boundary.context_frontier_id = NEW.context_frontier_id
                AND boundary.prefix_context_frontier_id = call.context_frontier_id
            WHERE call.session_id = NEW.session_id AND call.state_kind = ''terminal''
                AND call.terminal_disposition_kind = ''known_failed''
        UNION SELECT seed_context_frontier_id FROM imported_session_seed WHERE session_id = NEW.session_id');
    EXECUTE definition;
END;
$migration$;
