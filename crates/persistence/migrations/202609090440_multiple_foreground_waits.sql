-- Historical tool attempts remain in the current turn's continued-attempt chain.
DO $migration$
DECLARE
    definition text;
BEGIN
    SELECT pg_get_functiondef('assert_tool_loop_turn_final_state_pre_delegation(uuid)'::regprocedure)
      INTO definition;
    definition := replace(definition, $prior$                                        SELECT 1
                                          FROM turn_attempt AS resumed
                                         WHERE resumed.turn_attempt_id =
                                               lifecycle.current_attempt_id
                                           AND resumed.turn_id = lifecycle.turn_id
                                           AND resumed.session_id = lifecycle.session_id
                                           AND resumed.continued_from_attempt_id =
                                               attempt.issuing_turn_attempt_id$prior$, $current$                                        WITH RECURSIVE resumed AS (
                                            SELECT turn_attempt_id, continued_from_attempt_id
                                              FROM turn_attempt
                                             WHERE turn_attempt_id = lifecycle.current_attempt_id
                                               AND turn_id = lifecycle.turn_id
                                               AND session_id = lifecycle.session_id
                                            UNION
                                            SELECT predecessor.turn_attempt_id,
                                                   predecessor.continued_from_attempt_id
                                              FROM turn_attempt AS predecessor
                                              JOIN resumed AS successor
                                                ON predecessor.turn_attempt_id =
                                                   successor.continued_from_attempt_id
                                             WHERE predecessor.turn_id = lifecycle.turn_id
                                               AND predecessor.session_id = lifecycle.session_id
                                        )
                                        SELECT 1 FROM resumed
                                         WHERE resumed.continued_from_attempt_id =
                                               attempt.issuing_turn_attempt_id$current$);
    EXECUTE definition;
END;
$migration$;
