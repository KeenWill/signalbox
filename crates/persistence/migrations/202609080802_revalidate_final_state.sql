-- Retain each installed validator's full evidence checks while removing the
-- transaction-wide suppression of subsequent assertions.
DO $$
DECLARE
    signature text;
    definition text;
    revised text;
BEGIN
    FOREACH signature IN ARRAY ARRAY[
        'assert_turn_lifecycle_final_state(uuid)',
        'assert_model_call_final_state(uuid)',
        'assert_tool_round_final_state(uuid)'
    ] LOOP
        SELECT pg_get_functiondef(signature::regprocedure) INTO definition;
        revised := regexp_replace(
            definition,
            '\s*IF NOT claim_deferred_final_state_validation\([^;]+\) THEN\s+RETURN;\s+END IF;',
            ''
        );
        IF revised = definition THEN
            RAISE EXCEPTION 'final-state validator % has no claim check', signature;
        END IF;
        EXECUTE revised;
    END LOOP;
END
$$;

DROP FUNCTION claim_deferred_final_state_validation(text, uuid);
