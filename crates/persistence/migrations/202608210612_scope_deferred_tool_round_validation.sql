-- Tool rounds and their correlated evidence are immutable after construction,
-- and every table that can construct one already queues the round-specific
-- final-state validator. Rechecking every historical round whenever the owning
-- turn changes makes an otherwise constant-size continuation commit scan the
-- turn's entire frontier history while it holds the global outbox allocator.

DO $migration$
DECLARE
    checked_function regprocedure;
    definition text;
    updated_definition text;
    declaration text := E'    round_id uuid;\nBEGIN';
    round_scan text := E'    FOR round_id IN\n        SELECT producing_model_call_id\n          FROM tool_round\n         WHERE turn_id = lifecycle.turn_id\n           AND session_id = lifecycle.session_id\n    LOOP\n        PERFORM assert_tool_round_final_state(round_id);\n    END LOOP;\n\n';
BEGIN
    FOREACH checked_function IN ARRAY ARRAY[
        'assert_tool_loop_turn_final_state_pre_delegation(uuid)'::regprocedure,
        'assert_tool_loop_turn_final_state(uuid)'::regprocedure
    ]
    LOOP
        SELECT pg_get_functiondef(checked_function) INTO definition;
        updated_definition := replace(definition, declaration, 'BEGIN');
        IF updated_definition = definition THEN
            RAISE EXCEPTION
                'tool-round validation scope could not locate % declaration',
                checked_function;
        END IF;
        definition := updated_definition;
        updated_definition := replace(definition, round_scan, '');
        IF updated_definition = definition THEN
            RAISE EXCEPTION
                'tool-round validation scope could not locate % history scan',
                checked_function;
        END IF;
        EXECUTE updated_definition;
    END LOOP;
END;
$migration$;
