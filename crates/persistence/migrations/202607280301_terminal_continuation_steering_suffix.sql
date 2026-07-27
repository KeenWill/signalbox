-- A failed or cancelled turn may name a steering-consuming continuation call.
--
-- The tool-loop continuation transaction consumes pending steering after the
-- round's proposal-ordered results, so a later failure or cancellation of
-- that prepared or issued continuation call terminalizes with the consumed
-- steering entries between the result window and the terminal marker.
-- The turn-level terminal result-suffix law previously demanded the result
-- window immediately before the marker and rejected that legal writer shape
-- at commit. The law now accepts exactly the alternative: the named terminal
-- call is this turn's continuation-chain call, at least one consumed steering
-- input names it, and the terminal frontier extends that call's frontier —
-- itself re-validated by assert_model_call_steering_final_state in the same
-- deferred pass — by exactly the terminal marker. Every other shape keeps the
-- prior strict window.

CREATE OR REPLACE FUNCTION assert_tool_loop_turn_final_state(
    checked_turn_id uuid
)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    lifecycle turn_lifecycle%ROWTYPE;
    attempt_count bigint;
    initial_attempt_count bigint;
    linked_attempt_count bigint;
    live_attempt_count bigint;
    unresolved_result_count bigint;
    completion_count bigint;
    failure_count bigint;
    cancellation_count bigint;
    terminal_member_count numeric(20, 0);
    terminal_marker uuid;
    matching_terminal_round_count bigint;
    round_id uuid;
BEGIN
    SELECT *
      INTO lifecycle
      FROM turn_lifecycle
     WHERE turn_id = checked_turn_id;
    IF NOT FOUND THEN
        RETURN;
    END IF;

    FOR round_id IN
        SELECT producing_model_call_id
          FROM tool_round
         WHERE turn_id = lifecycle.turn_id
           AND session_id = lifecycle.session_id
    LOOP
        PERFORM assert_tool_round_final_state(round_id);
    END LOOP;

    SELECT
        count(*),
        count(*) FILTER (WHERE continued_from_attempt_id IS NULL),
        count(*) FILTER (WHERE continued_from_attempt_id IS NOT NULL),
        count(*) FILTER (WHERE state_kind <> 'ended')
      INTO
        attempt_count,
        initial_attempt_count,
        linked_attempt_count,
        live_attempt_count
      FROM turn_attempt
     WHERE turn_id = lifecycle.turn_id
       AND session_id = lifecycle.session_id;

    IF lifecycle.attempt_history_present IS DISTINCT FROM (attempt_count > 0)
       OR initial_attempt_count <> 1
       OR linked_attempt_count <> attempt_count - 1
    THEN
        RAISE EXCEPTION 'tool-loop turn lacks one linear attempt history'
            USING ERRCODE = '23514';
    END IF;

    IF lifecycle.state_kind = 'active' THEN
        IF EXISTS (
            SELECT 1
              FROM semantic_transcript_entry
             WHERE source_session_id = lifecycle.session_id
               AND (
                    failed_turn_id = lifecycle.turn_id
                    OR completed_turn_id = lifecycle.turn_id
                    OR cancelled_turn_id = lifecycle.turn_id
               )
               AND payload_kind IN (
                    'turn_failed',
                    'turn_completed',
                    'turn_cancelled'
               )
        ) THEN
            RAISE EXCEPTION 'active tool-loop turn carries a terminal marker'
                USING ERRCODE = '23514';
        END IF;

        CASE lifecycle.active_phase_kind
            WHEN 'running' THEN
                IF live_attempt_count <> 1
                   OR NOT EXISTS (
                        SELECT 1
                          FROM turn_attempt
                         WHERE turn_attempt_id = lifecycle.current_attempt_id
                           AND turn_id = lifecycle.turn_id
                           AND session_id = lifecycle.session_id
                           AND state_kind <> 'ended'
                   )
                THEN
                    RAISE EXCEPTION
                        'running tool-loop turn lacks its exact live attempt'
                        USING ERRCODE = '23514';
                END IF;

                IF lifecycle.active_tool_round_call_id IS NOT NULL
                   AND (
                        EXISTS (
                            SELECT 1
                              FROM tool_request AS request
                              LEFT JOIN tool_approval_decision AS approval
                                ON approval.request_id = request.request_id
                             WHERE request.producing_model_call_id =
                                   lifecycle.active_tool_round_call_id
                               AND approval.request_id IS NULL
                        )
                        OR EXISTS (
                            SELECT 1
                              FROM tool_attempt AS attempt
                              JOIN tool_request AS request
                                ON request.request_id = attempt.request_id
                             WHERE request.producing_model_call_id =
                                   lifecycle.active_tool_round_call_id
                               AND attempt.issuing_turn_attempt_id
                                   <> lifecycle.current_attempt_id
                        )
                   )
                THEN
                    RAISE EXCEPTION
                        'executing tool batch lacks resolved serial authority'
                        USING ERRCODE = '23514';
                END IF;
            WHEN 'awaiting_tool_approval' THEN
                IF live_attempt_count <> 0
                   OR EXISTS (
                        SELECT 1
                          FROM tool_attempt AS attempt
                          JOIN tool_request AS request
                            ON request.request_id = attempt.request_id
                         WHERE request.producing_model_call_id =
                               lifecycle.active_tool_round_call_id
                   )
                   OR NOT EXISTS (
                        SELECT 1
                          FROM tool_request AS waiting
                          LEFT JOIN tool_approval_decision AS approval
                            ON approval.request_id = waiting.request_id
                         WHERE waiting.request_id =
                               lifecycle.approval_tool_request_id
                           AND waiting.producing_model_call_id =
                               lifecycle.active_tool_round_call_id
                           AND approval.request_id IS NULL
                           AND NOT EXISTS (
                                SELECT 1
                                  FROM tool_request AS earlier
                                  LEFT JOIN tool_approval_decision AS earlier_approval
                                    ON earlier_approval.request_id =
                                       earlier.request_id
                                 WHERE earlier.producing_model_call_id =
                                       waiting.producing_model_call_id
                                   AND earlier.request_ordinal <
                                       waiting.request_ordinal
                                   AND earlier_approval.request_id IS NULL
                           )
                   )
                THEN
                    RAISE EXCEPTION
                        'approval wait is not the earliest undecided request'
                        USING
                            ERRCODE = '23514',
                            CONSTRAINT =
                                'tool_approval_wait_earliest_undecided';
                END IF;
            WHEN 'awaiting_tool_recovery' THEN
                IF live_attempt_count <> 0
                   OR NOT EXISTS (
                        SELECT 1
                          FROM tool_attempt AS attempt
                          JOIN tool_request AS request
                            ON request.request_id = attempt.request_id
                         WHERE attempt.attempt_id =
                               lifecycle.recovery_tool_attempt_id
                           AND request.producing_model_call_id =
                               lifecycle.active_tool_round_call_id
                           AND attempt.issuing_turn_attempt_id =
                               lifecycle.current_attempt_id
                           AND attempt.state_kind = 'terminal'
                           AND attempt.terminal_disposition_kind = 'ambiguous'
                   )
                   OR NOT EXISTS (
                        SELECT 1
                          FROM turn_attempt
                         WHERE turn_attempt_id = lifecycle.current_attempt_id
                           AND turn_id = lifecycle.turn_id
                           AND session_id = lifecycle.session_id
                           AND state_kind = 'ended'
                           AND end_disposition IN ('ambiguous', 'lost')
                   )
                THEN
                    RAISE EXCEPTION 'tool recovery wait lacks exact ambiguity'
                        USING ERRCODE = '23514';
                END IF;
            WHEN 'awaiting_model_call_recovery' THEN
                IF live_attempt_count <> 0
                   OR NOT EXISTS (
                        SELECT 1
                          FROM model_call
                         WHERE model_call_id =
                               lifecycle.recovery_model_call_id
                           AND turn_attempt_id =
                               lifecycle.current_attempt_id
                           AND turn_id = lifecycle.turn_id
                           AND session_id = lifecycle.session_id
                           AND state_kind = 'terminal'
                           AND terminal_disposition_kind = 'ambiguous'
                   )
                THEN
                    RAISE EXCEPTION
                        'model recovery wait lacks exact ambiguity'
                        USING ERRCODE = '23514';
                END IF;
            ELSE
                RAISE EXCEPTION 'unsupported active tool-loop phase'
                    USING ERRCODE = '23514';
        END CASE;
        RETURN;
    END IF;

    IF lifecycle.state_kind <> 'terminal' OR live_attempt_count <> 0 THEN
        RAISE EXCEPTION 'tool-loop turn is neither active nor terminal'
            USING ERRCODE = '23514';
    END IF;

    SELECT count(*)
      INTO completion_count
      FROM semantic_transcript_entry
     WHERE source_session_id = lifecycle.session_id
       AND payload_kind = 'turn_completed'
       AND completed_turn_id = lifecycle.turn_id;
    SELECT count(*)
      INTO failure_count
      FROM semantic_transcript_entry
     WHERE source_session_id = lifecycle.session_id
       AND payload_kind = 'turn_failed'
       AND failed_turn_id = lifecycle.turn_id;
    SELECT count(*)
      INTO cancellation_count
      FROM semantic_transcript_entry
     WHERE source_session_id = lifecycle.session_id
       AND payload_kind = 'turn_cancelled'
       AND cancelled_turn_id = lifecycle.turn_id;

    IF (
        lifecycle.terminal_disposition_kind = 'completed'
        AND (
            completion_count <> 1
            OR failure_count <> 0
            OR cancellation_count <> 0
        )
    ) OR (
        lifecycle.terminal_disposition_kind = 'failed'
        AND (
            failure_count <> 1
            OR completion_count <> 0
            OR cancellation_count <> 0
        )
    ) OR (
        lifecycle.terminal_disposition_kind = 'cancelled'
        AND (
            cancellation_count <> 1
            OR completion_count <> 0
            OR failure_count <> 0
        )
    ) THEN
        RAISE EXCEPTION 'tool-loop terminal marker contradicts disposition'
            USING ERRCODE = '23514';
    END IF;

    IF lifecycle.terminal_disposition_kind IN ('failed', 'cancelled') THEN
        SELECT semantic_entry_id
          INTO terminal_marker
          FROM semantic_transcript_entry
         WHERE source_session_id = lifecycle.session_id
           AND (
                (
                    payload_kind = 'turn_failed'
                    AND failed_turn_id = lifecycle.turn_id
                )
                OR (
                    payload_kind = 'turn_cancelled'
                    AND cancelled_turn_id = lifecycle.turn_id
                )
           );
        SELECT member_count
          INTO terminal_member_count
          FROM context_frontier
         WHERE owning_session_id = lifecycle.session_id
           AND context_frontier_id = lifecycle.terminal_frontier_id;
        IF NOT EXISTS (
            SELECT 1
              FROM context_frontier_member
             WHERE owning_session_id = lifecycle.session_id
               AND context_frontier_id = lifecycle.terminal_frontier_id
               AND member_position = terminal_member_count
               AND source_session_id = lifecycle.session_id
               AND semantic_entry_id = terminal_marker
        ) THEN
            RAISE EXCEPTION
                'failed or cancelled tool-loop terminal marker is not last'
                USING
                    ERRCODE = '23514',
                    CONSTRAINT = 'tool_loop_terminal_result_suffix_exact';
        END IF;

        IF EXISTS (
            SELECT 1
              FROM context_frontier_member AS member
              JOIN semantic_transcript_entry AS entry
                ON entry.source_session_id = member.source_session_id
               AND entry.semantic_entry_id = member.semantic_entry_id
              LEFT JOIN tool_attempt AS attempt
                ON attempt.attempt_id = entry.tool_result_attempt_id
              JOIN tool_request AS request
                ON request.turn_id = lifecycle.turn_id
               AND request.session_id = lifecycle.session_id
               AND (
                    request.request_id = entry.tool_result_request_id
                    OR request.request_id = attempt.request_id
               )
             WHERE member.owning_session_id = lifecycle.session_id
               AND member.context_frontier_id =
                   lifecycle.terminal_frontier_id
               AND entry.payload_kind IN (
                    'tool_execution_result',
                    'tool_denied',
                    'tool_closed_by_turn_end'
               )
        ) THEN
            SELECT count(*)
              INTO matching_terminal_round_count
              FROM tool_round AS round
             WHERE round.turn_id = lifecycle.turn_id
               AND round.session_id = lifecycle.session_id
               AND NOT EXISTS (
                    SELECT 1
                      FROM tool_request AS request
                      LEFT JOIN semantic_transcript_entry AS result
                        ON result.source_session_id = lifecycle.session_id
                       AND result.payload_kind IN (
                            'tool_execution_result',
                            'tool_denied',
                            'tool_closed_by_turn_end'
                       )
                       AND (
                            result.tool_result_request_id =
                                request.request_id
                            OR EXISTS (
                                SELECT 1
                                  FROM tool_attempt AS result_attempt
                                 WHERE result_attempt.attempt_id =
                                       result.tool_result_attempt_id
                                   AND result_attempt.request_id =
                                       request.request_id
                            )
                       )
                      LEFT JOIN context_frontier_member AS member
                        ON member.owning_session_id =
                           lifecycle.session_id
                       AND member.context_frontier_id =
                           lifecycle.terminal_frontier_id
                       AND member.member_position = (
                            terminal_member_count
                            - round.request_count
                            + request.request_ordinal
                       )
                       AND member.source_session_id =
                           result.source_session_id
                       AND member.semantic_entry_id =
                           result.semantic_entry_id
                     WHERE request.producing_model_call_id =
                           round.producing_model_call_id
                       AND member.semantic_entry_id IS NULL
               )
               AND (
                    round.boundary_kind = 'closed_by_turn_end'
                    OR (
                        SELECT member_count
                          FROM context_frontier
                         WHERE owning_session_id = lifecycle.session_id
                           AND context_frontier_id =
                               round.boundary_frontier_id
                    ) = terminal_member_count - round.request_count - 1
               )
               AND (
                    round.boundary_kind = 'closed_by_turn_end'
                    OR NOT EXISTS (
                        (
                            SELECT
                                member_position,
                                source_session_id,
                                semantic_entry_id
                              FROM context_frontier_member
                             WHERE owning_session_id =
                                   lifecycle.session_id
                               AND context_frontier_id =
                                   round.boundary_frontier_id
                            EXCEPT
                            SELECT
                                member_position,
                                source_session_id,
                                semantic_entry_id
                              FROM context_frontier_member
                             WHERE owning_session_id =
                                   lifecycle.session_id
                               AND context_frontier_id =
                                   lifecycle.terminal_frontier_id
                               AND member_position <
                                   terminal_member_count
                                   - round.request_count
                        )
                    )
               );
            IF matching_terminal_round_count <> 1
               AND NOT (
                    lifecycle.terminal_model_call_id IS NOT NULL
                    AND EXISTS (
                        SELECT 1
                          FROM accepted_input AS consumed
                         WHERE consumed.session_id = lifecycle.session_id
                           AND consumed.expected_active_turn_id =
                               lifecycle.turn_id
                           AND consumed.disposition_kind =
                               'consumed_as_steering'
                           AND consumed.consuming_model_call_id =
                               lifecycle.terminal_model_call_id
                    )
                    AND EXISTS (
                        SELECT 1
                          FROM model_call AS named
                          JOIN turn_attempt AS named_attempt
                            ON named_attempt.turn_attempt_id =
                               named.turn_attempt_id
                           AND named_attempt.turn_id = named.turn_id
                           AND named_attempt.session_id = named.session_id
                          JOIN context_frontier AS named_frontier
                            ON named_frontier.owning_session_id =
                               named.session_id
                           AND named_frontier.context_frontier_id =
                               named.context_frontier_id
                         WHERE named.model_call_id =
                               lifecycle.terminal_model_call_id
                           AND named.turn_id = lifecycle.turn_id
                           AND named.session_id = lifecycle.session_id
                           AND named.state_kind = 'terminal'
                           AND named_attempt.continued_from_attempt_id
                               IS NOT NULL
                           AND named_frontier.member_count =
                               terminal_member_count - 1
                           AND NOT EXISTS (
                                SELECT 1
                                  FROM context_frontier_member AS named_member
                                  LEFT JOIN context_frontier_member
                                    AS terminal_member
                                    ON terminal_member.owning_session_id =
                                       named_member.owning_session_id
                                   AND terminal_member.context_frontier_id =
                                       lifecycle.terminal_frontier_id
                                   AND terminal_member.member_position =
                                       named_member.member_position
                                 WHERE named_member.owning_session_id =
                                       lifecycle.session_id
                                   AND named_member.context_frontier_id =
                                       named.context_frontier_id
                                   AND ROW(
                                        terminal_member.source_session_id,
                                        terminal_member.semantic_entry_id
                                   ) IS DISTINCT FROM ROW(
                                        named_member.source_session_id,
                                        named_member.semantic_entry_id
                                   )
                           )
                    )
               )
            THEN
                RAISE EXCEPTION
                    'failed or cancelled tool-loop turn lacks its exact terminal result suffix'
                    USING
                        ERRCODE = '23514',
                        CONSTRAINT =
                            'tool_loop_terminal_result_suffix_exact';
            END IF;
        END IF;
    END IF;

    IF lifecycle.terminal_disposition_kind IN (
        'completed',
        'failed',
        'cancelled',
        'reconciliation_required'
    ) THEN
        SELECT count(*)
          INTO unresolved_result_count
          FROM tool_request AS request
         WHERE request.turn_id = lifecycle.turn_id
           AND request.session_id = lifecycle.session_id
           AND NOT EXISTS (
                SELECT 1
                  FROM semantic_transcript_entry AS entry
                  LEFT JOIN tool_attempt AS attempt
                    ON attempt.attempt_id = entry.tool_result_attempt_id
                 WHERE entry.source_session_id = lifecycle.session_id
                   AND (
                        entry.tool_result_request_id = request.request_id
                        OR attempt.request_id = request.request_id
                   )
                   AND entry.payload_kind IN (
                        'tool_execution_result',
                        'tool_denied',
                        'tool_closed_by_turn_end'
                   )
           );
        IF unresolved_result_count <> 0 THEN
            RAISE EXCEPTION 'terminal tool-loop turn has unresolved requests'
                USING ERRCODE = '23514';
        END IF;
    END IF;
END;
$$;
