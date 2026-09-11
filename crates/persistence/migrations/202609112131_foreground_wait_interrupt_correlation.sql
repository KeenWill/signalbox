DO $migration$
DECLARE
    prior_definition text;
    next_definition text;
BEGIN
    prior_definition := pg_get_functiondef(
        'require_interrupt_submit_input_effect_correlation()'::regprocedure
    );
    next_definition := replace(
        prior_definition,
        'AND cancelled.terminal_model_call_id IS NULL',
        'AND cancelled.terminal_model_call_id IS NULL
                           AND EXISTS (
                                SELECT 1
                                  FROM resolve_context_frontier_members(
                                      cancelled.session_id,
                                      cancelled.terminal_frontier_id
                                  ) AS member
                                  JOIN semantic_transcript_entry AS entry
                                    ON entry.source_session_id = member.source_session_id
                                   AND entry.semantic_entry_id = member.semantic_entry_id
                                 WHERE entry.payload_kind = ''tool_closed_by_turn_end''
                                   AND entry.tool_result_request_id =
                                        waiting.awaiting_tool_request_id
                           )'
    );
    IF next_definition = prior_definition THEN
        RAISE EXCEPTION 'foreground wait interrupt definition did not match';
    END IF;
    EXECUTE next_definition;
END;
$migration$;
