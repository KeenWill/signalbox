DO $migration$
DECLARE
    prior_definition text;
    next_definition text;
BEGIN
    prior_definition := pg_get_functiondef(
        'require_interrupt_submit_input_effect_correlation()'::regprocedure
    );
    next_definition := replace(prior_definition, $prior$                           AND EXISTS (
                                SELECT 1
                                  FROM resolve_context_frontier_members(
                                      cancelled.session_id,
                                      cancelled.terminal_frontier_id
                                  ) AS member
                                  JOIN semantic_transcript_entry AS entry
                                    ON entry.source_session_id = member.source_session_id
                                   AND entry.semantic_entry_id = member.semantic_entry_id
                                 WHERE entry.payload_kind = 'tool_closed_by_turn_end'
                                   AND entry.tool_result_request_id =
                                        waiting.awaiting_tool_request_id
                           )$prior$, $next$$next$);
    IF next_definition = prior_definition THEN
        RAISE EXCEPTION 'foreground interrupt definition did not match';
    END IF;
    prior_definition := next_definition;
    next_definition := replace(prior_definition, $prior$                          JOIN model_call AS producing_call
                            ON producing_call.model_call_id =
                                awaiting.producing_model_call_id
                           AND producing_call.turn_id = awaiting.turn_id
                           AND producing_call.session_id = awaiting.session_id$prior$, $next$                          JOIN tool_attempt AS waiting_attempt
                            ON waiting_attempt.request_id = awaiting.request_id
                           AND waiting_attempt.turn_id = awaiting.turn_id
                           AND waiting_attempt.session_id = awaiting.session_id
                           AND waiting_attempt.state_kind = 'terminal'
                           AND waiting_attempt.terminal_disposition_kind = 'awaiting_child'$next$);
    IF next_definition = prior_definition THEN
        RAISE EXCEPTION 'foreground interrupt definition did not match';
    END IF;
    prior_definition := next_definition;
    next_definition := replace(prior_definition, $prior$                           AND producing_call.turn_attempt_id =
                                stopped_attempt.turn_attempt_id$prior$, $next$                           AND waiting_attempt.issuing_turn_attempt_id =
                                stopped_attempt.turn_attempt_id
                           AND NOT EXISTS (
                                SELECT 1 FROM turn_attempt AS continuation
                                 WHERE continuation.continued_from_attempt_id =
                                        stopped_attempt.turn_attempt_id
                           )$next$);
    IF next_definition = prior_definition THEN
        RAISE EXCEPTION 'foreground interrupt definition did not match';
    END IF;
    prior_definition := next_definition;
    EXECUTE next_definition;
END;
$migration$;
