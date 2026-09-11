DO $migration$
DECLARE
    definition text;
    old_fragment text := $old$        IF EXISTS (
            SELECT 1 FROM session_child_result AS result
             WHERE result.spawning_tool_request_id =
                    frontier.spawning_tool_request_id
        ) THEN
$old$;
    new_fragment text := $new$        IF EXISTS (
            SELECT 1 FROM session_child_result AS result
             WHERE result.spawning_tool_request_id =
                    frontier.spawning_tool_request_id
        ) OR EXISTS (
            SELECT 1
              FROM session_delegation_initial_task AS task
              JOIN turn_lifecycle AS lifecycle
                ON lifecycle.turn_id = task.turn_id
               AND lifecycle.session_id = task.child_session_id
             WHERE task.spawning_tool_request_id =
                    frontier.spawning_tool_request_id
               AND lifecycle.state_kind = 'terminal'
               AND lifecycle.terminal_disposition_kind =
                    'reconciliation_required'
        ) THEN
$new$;
BEGIN
    definition := pg_get_functiondef(
        'materialize_session_delegation_termination_cascade(uuid, text)'::regprocedure
    );
    IF position(old_fragment IN definition) = 0 THEN
        RAISE EXCEPTION 'cascade terminal predicate was not found';
    END IF;
    EXECUTE replace(definition, old_fragment, new_fragment);
END;
$migration$;

DO $migration$
DECLARE
    definition text;
    old_fragment text := $old$            IF NEW.outcome_kind = 'already_terminal' AND NOT EXISTS (
                SELECT 1 FROM session_child_result AS prior
                 WHERE prior.spawning_tool_request_id = NEW.spawning_tool_request_id
                   AND prior.event_ordinal < NEW.event_ordinal
            ) THEN
$old$;
    new_fragment text := $new$            IF NEW.outcome_kind = 'already_terminal'
               AND NOT EXISTS (
                    SELECT 1 FROM session_child_result AS prior
                     WHERE prior.spawning_tool_request_id = NEW.spawning_tool_request_id
                       AND prior.event_ordinal < NEW.event_ordinal
               )
               AND NOT EXISTS (
                    SELECT 1
                      FROM session_delegation_initial_task AS task
                      JOIN turn_lifecycle AS lifecycle
                        ON lifecycle.turn_id = task.turn_id
                       AND lifecycle.session_id = task.child_session_id
                     WHERE task.spawning_tool_request_id =
                            NEW.spawning_tool_request_id
                       AND lifecycle.state_kind = 'terminal'
                       AND lifecycle.terminal_disposition_kind =
                            'reconciliation_required'
               ) THEN
$new$;
BEGIN
    definition := pg_get_functiondef(
        'require_session_delegation_event_payload()'::regprocedure
    );
    IF position(old_fragment IN definition) = 0 THEN
        RAISE EXCEPTION 'already-terminal evidence predicate was not found';
    END IF;
    EXECUTE replace(definition, old_fragment, new_fragment);
END;
$migration$;
