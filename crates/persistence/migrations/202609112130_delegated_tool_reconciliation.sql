DO $migration$
DECLARE
    function_oid regprocedure;
    prior_definition text;
    next_definition text;
BEGIN
    FOREACH function_oid IN ARRAY ARRAY[
        'require_session_delegation_event_payload()'::regprocedure,
        'require_terminal_delegated_turn_result()'::regprocedure
    ] LOOP
        prior_definition := pg_get_functiondef(function_oid);
        next_definition := replace(
            prior_definition,
            'recovery.model_call_id = lifecycle.terminal_model_call_id',
            '(recovery.model_call_id = lifecycle.terminal_model_call_id
                OR recovery.tool_attempt_id = lifecycle.terminal_tool_attempt_id)'
        );
        IF next_definition = prior_definition THEN
            RAISE EXCEPTION 'delegated reconciliation definition did not match: %', function_oid;
        END IF;
        EXECUTE next_definition;
    END LOOP;
END;
$migration$;
