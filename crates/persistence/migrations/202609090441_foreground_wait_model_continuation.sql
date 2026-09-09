CREATE FUNCTION tool_round_call_from_predecessor(checked_predecessor uuid, checked_turn uuid, checked_session uuid)
RETURNS uuid LANGUAGE sql STABLE AS $$
    WITH RECURSIVE history AS (
        SELECT turn_attempt_id, continued_from_attempt_id
          FROM turn_attempt
         WHERE turn_attempt_id = checked_predecessor
           AND turn_id = checked_turn AND session_id = checked_session
        UNION
        SELECT predecessor.turn_attempt_id, predecessor.continued_from_attempt_id
          FROM turn_attempt AS predecessor
          JOIN history AS successor
            ON predecessor.turn_attempt_id = successor.continued_from_attempt_id
         WHERE predecessor.turn_id = checked_turn AND predecessor.session_id = checked_session
           AND NOT EXISTS (SELECT 1 FROM model_call WHERE turn_attempt_id = successor.turn_attempt_id)
           AND EXISTS (
                SELECT 1 FROM tool_attempt AS waiting
                 WHERE waiting.issuing_turn_attempt_id = successor.turn_attempt_id
                   AND waiting.turn_id = checked_turn AND waiting.session_id = checked_session
                   AND waiting.state_kind = 'terminal'
                   AND waiting.terminal_disposition_kind = 'awaiting_child'
           )
    )
    SELECT call.model_call_id
      FROM history JOIN model_call AS call USING (turn_attempt_id)
      JOIN tool_round AS round ON round.producing_model_call_id = call.model_call_id
     WHERE call.turn_id = checked_turn AND call.session_id = checked_session
       AND call.state_kind = 'terminal' AND call.terminal_disposition_kind = 'completed'
       AND round.boundary_kind = 'continuing';
$$;

DO $migration$
DECLARE definition text;
BEGIN
    SELECT pg_get_functiondef('assert_model_call_steering_final_state(uuid)'::regprocedure)
      INTO definition;
    definition := replace(definition,
        'WHERE producing_call.turn_attempt_id = predecessor_attempt',
        'WHERE producing_call.model_call_id = tool_round_call_from_predecessor(
               predecessor_attempt, checked_turn, checked_session)');
    EXECUTE definition;

    SELECT pg_get_functiondef('continuation_frontier_closes_predecessor_tool_round(uuid,uuid,uuid,uuid)'::regprocedure)
      INTO definition;
    definition := replace(definition,
        'predecessor_call.turn_attempt_id =
               continuation_attempt.continued_from_attempt_id',
        'predecessor_call.model_call_id = tool_round_call_from_predecessor(
               continuation_attempt.continued_from_attempt_id,
               continuation_attempt.turn_id, continuation_attempt.session_id)');
    EXECUTE definition;
END;
$migration$;
