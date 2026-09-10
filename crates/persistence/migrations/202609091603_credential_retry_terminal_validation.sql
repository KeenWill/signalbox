DO $migration$
DECLARE
    definition text;
BEGIN
    SELECT pg_get_functiondef('credential_wait_terminal_history_is_valid(uuid)'::regprocedure)
      INTO definition;
    definition := replace(definition,
        $prior$AND EXISTS (SELECT 1 FROM history attempt
            JOIN credential_availability_wait_release release USING (turn_attempt_id)
            JOIN credential_availability_wait waiting USING (wait_attempt_id)
            WHERE waiting.consumed_by_attempt_id = attempt.turn_attempt_id
              AND attempt.continued_from_attempt_id = waiting.wait_attempt_id)$prior$,
        $updated$AND (
            EXISTS (SELECT 1 FROM history attempt
                JOIN credential_availability_wait_release release USING (turn_attempt_id)
                JOIN credential_availability_wait waiting USING (wait_attempt_id)
                WHERE waiting.consumed_by_attempt_id = attempt.turn_attempt_id
                  AND attempt.continued_from_attempt_id = waiting.wait_attempt_id)
            OR EXISTS (SELECT 1 FROM history attempt
                JOIN credential_pool_availability_successor successor
                  ON successor.successor_turn_attempt_id = attempt.turn_attempt_id
                JOIN model_call predecessor
                  ON predecessor.model_call_id = successor.predecessor_model_call_id
                 AND predecessor.turn_attempt_id = attempt.continued_from_attempt_id
                 AND predecessor.turn_id = attempt.turn_id
                 AND predecessor.session_id = attempt.session_id)
        )$updated$);
    EXECUTE definition;

    SELECT pg_get_functiondef('assert_model_call_steering_final_state(uuid)'::regprocedure)
      INTO definition;
    definition := replace(definition,
        'SELECT member_count
      INTO starting_count',
        'SELECT predecessor.context_frontier_id INTO result_boundary
           FROM model_call AS call
           JOIN credential_pool_availability_successor AS successor
             ON successor.successor_turn_attempt_id = call.turn_attempt_id
           JOIN model_call AS predecessor
             ON predecessor.model_call_id = successor.predecessor_model_call_id
            AND predecessor.session_id = call.session_id
            AND predecessor.turn_id = call.turn_id
          WHERE call.model_call_id = checked_model_call_id;
        IF FOUND THEN
            starting_frontier := result_boundary;
            predecessor_attempt := NULL;
        END IF;

        SELECT member_count
      INTO starting_count');
    EXECUTE definition;
END;
$migration$;
