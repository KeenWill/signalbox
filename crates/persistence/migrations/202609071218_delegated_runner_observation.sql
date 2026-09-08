DO $$
DECLARE definition text;
BEGIN
    SELECT pg_get_functiondef('assert_model_call_final_state(uuid)'::regprocedure) INTO definition;
    definition := replace(definition, 'BEGIN' || chr(10), 'BEGIN
    IF EXISTS (
        SELECT 1 FROM model_call AS call
        JOIN turn_lifecycle AS turn ON turn.turn_id = call.turn_id AND turn.session_id = call.session_id
        JOIN session_delegation_initial_task AS task ON task.child_session_id = call.session_id AND task.turn_id = call.turn_id
        JOIN session_delegation_logical_terminal AS terminal ON terminal.spawning_tool_request_id = task.spawning_tool_request_id
            AND terminal.child_session_id = task.child_session_id AND terminal.child_turn_id = task.turn_id
        WHERE call.model_call_id = checked_model_call_id AND call.state_kind = ''terminal''
            AND call.terminal_disposition_kind = ''cancelled'' AND turn.delegation_runtime_terminal
    ) THEN
        RETURN;
    END IF;
');
    EXECUTE definition;
END;
$$;

CREATE TRIGGER runner_recovery_after_model_observation
    AFTER UPDATE OF state_kind ON model_call
    FOR EACH ROW WHEN (OLD.state_kind IS DISTINCT FROM NEW.state_kind AND NEW.state_kind = 'terminal')
    EXECUTE FUNCTION notify_runner_recovery_authority_change();
