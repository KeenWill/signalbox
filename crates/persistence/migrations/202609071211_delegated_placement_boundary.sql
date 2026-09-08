DO $$
DECLARE definition text;
BEGIN
    SELECT pg_get_functiondef('require_runner_placement_boundary()'::regprocedure) INTO definition;
    definition := replace(definition,
        'FROM turn_lifecycle WHERE session_id = NEW.session_id AND state_kind = ''terminal''
              AND terminal_frontier_id IS NOT NULL',
        'FROM turn_lifecycle WHERE session_id = NEW.session_id
              AND turn_lifecycle_effective_terminal_frontier(session_id, turn_id) IS NOT NULL');
    EXECUTE definition;
END;
$$;
