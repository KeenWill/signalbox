-- The tool loop: tool requests, rounds, batches, and attempts; approval
-- decisions, user overrides, and the approval judge's model calls; the
-- decide/override commands; continuation context headroom; and the judge's
-- evaluation runs with the evaluation corpus they replay.

-- Function bodies may read tables a later section or file creates;
-- 202609010000_core.sql explains why validation is deferred.
SET check_function_bodies = false;

--
-- Functions.
--

--
-- Name: assert_failed_terminal_execution_before_context_headroom(uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION assert_failed_terminal_execution_before_context_headroom(checked_turn_id uuid) RETURNS void
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF EXISTS (
        SELECT 1
          FROM credential_pool_terminal_exhaustion AS exhausted
          JOIN turn_lifecycle AS lifecycle
            ON lifecycle.turn_id = exhausted.turn_id
           AND lifecycle.session_id = exhausted.session_id
          JOIN turn_attempt AS attempt
            ON attempt.turn_attempt_id = exhausted.terminal_attempt_id
           AND attempt.turn_id = exhausted.turn_id
           AND attempt.session_id = exhausted.session_id
          LEFT JOIN model_call AS call
            ON call.model_call_id = exhausted.terminal_model_call_id
           AND call.turn_attempt_id = exhausted.terminal_attempt_id
           AND call.turn_id = exhausted.turn_id
           AND call.session_id = exhausted.session_id
         WHERE exhausted.turn_id = checked_turn_id
           AND lifecycle.state_kind = 'terminal'
           AND lifecycle.terminal_disposition_kind = 'failed'
           AND lifecycle.terminal_attempt_id = exhausted.terminal_attempt_id
           AND lifecycle.terminal_model_call_id IS NOT DISTINCT FROM
               exhausted.terminal_model_call_id
           AND attempt.state_kind = 'ended'
           AND attempt.end_disposition = 'known_failure'
           AND (
                exhausted.terminal_model_call_id IS NULL
                OR (
                    call.state_kind = 'terminal'
                    AND call.terminal_disposition_kind = 'known_failed'
                )
           )
    ) THEN
        RETURN;
    END IF;

    PERFORM assert_failed_terminal_execution_before_credential_pools(
        checked_turn_id
    );
END;
$$;


--
-- Name: assert_failed_terminal_execution_without_tool_loop(uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION assert_failed_terminal_execution_without_tool_loop(checked_turn_id uuid) RETURNS void
    LANGUAGE plpgsql
    AS $$
DECLARE
    checked_session_id uuid;
    checked_terminal_attempt uuid;
    checked_terminal_call uuid;
    attempt_count bigint;
    call_count bigint;
BEGIN
    SELECT
        session_id,
        terminal_attempt_id,
        terminal_model_call_id
      INTO
        checked_session_id,
        checked_terminal_attempt,
        checked_terminal_call
      FROM turn_lifecycle
     WHERE turn_id = checked_turn_id
       AND state_kind = 'terminal'
       AND terminal_disposition_kind = 'failed';

    IF NOT FOUND THEN
        RETURN;
    END IF;

    SELECT count(*)
      INTO attempt_count
      FROM turn_attempt
     WHERE turn_id = checked_turn_id
       AND session_id = checked_session_id;

    SELECT count(*)
      INTO call_count
      FROM model_call
     WHERE turn_id = checked_turn_id
       AND session_id = checked_session_id;

    IF attempt_count = 0 THEN
        IF checked_terminal_attempt IS NOT NULL
           OR checked_terminal_call IS NOT NULL
           OR call_count <> 0
        THEN
            RAISE EXCEPTION 'direct failed turn % carries execution provenance', checked_turn_id
                USING ERRCODE = '23514';
        END IF;
        RETURN;
    END IF;

    IF attempt_count <> 1
       OR checked_terminal_attempt IS NULL
       OR NOT EXISTS (
            SELECT 1
              FROM turn_attempt
             WHERE turn_attempt_id = checked_terminal_attempt
               AND turn_id = checked_turn_id
               AND session_id = checked_session_id
               AND state_kind = 'ended'
               AND end_variant = 'without_stop'
               AND end_disposition IN ('known_failure', 'lost')
       )
    THEN
        RAISE EXCEPTION 'failed turn % lacks its exact ended attempt', checked_turn_id
            USING ERRCODE = '23514';
    END IF;

    IF call_count = 0 THEN
        IF checked_terminal_call IS NOT NULL THEN
            RAISE EXCEPTION 'failed turn % names an absent terminal call', checked_turn_id
                USING ERRCODE = '23514';
        END IF;
        RETURN;
    END IF;

    IF call_count <> 1
       OR checked_terminal_call IS NULL
       OR NOT EXISTS (
            SELECT 1
              FROM model_call
             WHERE model_call_id = checked_terminal_call
               AND turn_attempt_id = checked_terminal_attempt
               AND turn_id = checked_turn_id
               AND session_id = checked_session_id
               AND state_kind = 'terminal'
               AND terminal_disposition_kind IN ('known_failed', 'cancelled')
       )
    THEN
        RAISE EXCEPTION 'failed turn % lacks its exact terminal call', checked_turn_id
            USING ERRCODE = '23514';
    END IF;

    -- The model-call assertion independently verifies the frozen selection,
    -- turn-level target pin, starting frontier, owning attempt, and physical
    -- predecessor/disposition matrix.
    PERFORM assert_model_call_final_state(checked_terminal_call);
END;
$$;


--
-- Name: assert_tool_attempt_authorized(uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION assert_tool_attempt_authorized(checked_attempt_id uuid) RETURNS void
    LANGUAGE plpgsql
    AS $$
DECLARE
    checked_request uuid;
BEGIN
    SELECT request_id
      INTO checked_request
      FROM tool_attempt
     WHERE attempt_id = checked_attempt_id;
    IF NOT FOUND THEN
        RETURN;
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM tool_approval_decision
         WHERE request_id = checked_request
           AND decision_kind = 'approve'
    ) THEN
        RAISE EXCEPTION 'tool attempt lacks exact approval authority'
            USING
                ERRCODE = '23514',
                CONSTRAINT = 'tool_attempt_requires_approval';
    END IF;
END;
$$;


--
-- Name: assert_tool_decision_command_final_state(uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION assert_tool_decision_command_final_state(checked_command_id uuid) RETURNS void
    LANGUAGE plpgsql
    AS $$
DECLARE
    command_record decide_tool_request_command%ROWTYPE;
    approval_count bigint;
    earliest_correlation_count bigint;
BEGIN
    SELECT *
      INTO command_record
      FROM decide_tool_request_command
     WHERE command_id = checked_command_id;
    IF NOT FOUND THEN
        RETURN;
    END IF;

    SELECT count(*)
      INTO approval_count
      FROM tool_approval_decision AS approval
     WHERE approval.user_command_id = checked_command_id
       AND approval.request_id = command_record.request_id
       AND approval.decision_source IN ('user_command', 'lifecycle_closure')
       AND approval.decision_kind = command_record.decision_kind
       AND approval.denial_reason
           IS NOT DISTINCT FROM command_record.denial_reason;

    SELECT count(*)
      INTO earliest_correlation_count
      FROM tool_request AS requested
      JOIN tool_request AS earliest
        ON earliest.request_id =
           command_record.result_earliest_undecided_request_id
       AND earliest.producing_model_call_id =
           requested.producing_model_call_id
       AND earliest.request_ordinal < requested.request_ordinal
     WHERE requested.request_id = command_record.request_id;

    IF command_record.rejection_kind = 'not_earliest_undecided'
       AND earliest_correlation_count <> 1
    THEN
        RAISE EXCEPTION
            'tool decision command names an uncorrelated earlier request'
            USING
                ERRCODE = '23514',
                CONSTRAINT =
                    'decide_tool_request_command_earliest_correlation';
    END IF;

    IF (
        command_record.result_kind = 'applied'
        AND approval_count <> 1
    ) OR (
        command_record.result_kind = 'rejected'
        AND EXISTS (
            SELECT 1
              FROM tool_approval_decision
             WHERE user_command_id = checked_command_id
        )
    ) THEN
        RAISE EXCEPTION
            'tool decision command lacks its exact approval effect'
            USING ERRCODE = '23514';
    END IF;
END;
$$;


--
-- Name: assert_tool_loop_turn_final_state(uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION assert_tool_loop_turn_final_state(checked_turn_id uuid) RETURNS void
    LANGUAGE plpgsql
    AS $$
DECLARE
    lifecycle turn_lifecycle%ROWTYPE;
    attempt_count bigint;
    initial_attempt_count bigint;
    linked_attempt_count bigint;
    live_attempt_count bigint;
    matching_wait_count bigint;
BEGIN
    SELECT *
      INTO lifecycle
      FROM turn_lifecycle
     WHERE turn_id = checked_turn_id;
    IF NOT FOUND THEN
        RETURN;
    END IF;

    IF lifecycle.state_kind IS DISTINCT FROM 'active'
       OR lifecycle.active_phase_kind IS DISTINCT FROM 'awaiting_child'
    THEN
        PERFORM assert_tool_loop_turn_final_state_pre_delegation(
            checked_turn_id
        );
        RETURN;
    END IF;

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
       OR live_attempt_count <> 0
       OR EXISTS (
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
       )
    THEN
        RAISE EXCEPTION
            'child-wait tool-loop turn lacks one ended attempt history'
            USING ERRCODE = '23514';
    END IF;

    SELECT count(*)
      INTO matching_wait_count
      FROM session_delegation_wait AS child_wait
      JOIN tool_request AS request
        ON request.request_id = child_wait.awaiting_tool_request_id
      JOIN tool_attempt AS attempt
        ON attempt.request_id = request.request_id
       AND attempt.session_id = request.session_id
       AND attempt.turn_id = request.turn_id
      JOIN turn_attempt AS issuing_attempt
        ON issuing_attempt.turn_attempt_id = attempt.issuing_turn_attempt_id
       AND issuing_attempt.turn_id = attempt.turn_id
       AND issuing_attempt.session_id = attempt.session_id
     WHERE child_wait.awaiting_tool_request_id = lifecycle.child_wait_request_id
       AND child_wait.parent_turn_id = lifecycle.turn_id
       AND child_wait.parent_session_id = lifecycle.session_id
       AND child_wait.wait_mode = 'foreground'
       AND request.producing_model_call_id = lifecycle.active_tool_round_call_id
       AND attempt.state_kind = 'terminal'
       AND attempt.terminal_disposition_kind = 'awaiting_child'
       AND attempt.effect_class = 'effect_free'
       AND attempt.wait_spawning_request_id =
           child_wait.spawning_tool_request_id
       AND attempt.wait_child_session_id = child_wait.child_session_id
       AND issuing_attempt.state_kind = 'ended'
       AND issuing_attempt.end_variant = 'without_stop'
       AND issuing_attempt.end_disposition = 'yielded_to_durable_wait';

    IF matching_wait_count <> 1 THEN
        RAISE EXCEPTION
            'child wait lacks its exact ended await attempt and provenance'
            USING ERRCODE = '23514';
    END IF;
END;
$$;


--
-- Name: assert_tool_loop_turn_final_state_pre_delegation(uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION assert_tool_loop_turn_final_state_pre_delegation(checked_turn_id uuid) RETURNS void
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
BEGIN
    SELECT *
      INTO lifecycle
      FROM turn_lifecycle
     WHERE turn_id = checked_turn_id;
    IF NOT FOUND THEN
        RETURN;
    END IF;

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
                               AND NOT (
                                    EXISTS (
                                        SELECT 1
                                          FROM turn_attempt AS resumed
                                         WHERE resumed.turn_attempt_id =
                                               lifecycle.current_attempt_id
                                           AND resumed.turn_id = lifecycle.turn_id
                                           AND resumed.session_id = lifecycle.session_id
                                           AND resumed.continued_from_attempt_id =
                                               attempt.issuing_turn_attempt_id
                                    )
                                    AND EXISTS (
                                        SELECT 1
                                          FROM tool_attempt AS child_wait
                                          JOIN tool_request AS waited_request
                                            ON waited_request.request_id =
                                               child_wait.request_id
                                         WHERE waited_request.producing_model_call_id =
                                               lifecycle.active_tool_round_call_id
                                           AND child_wait.issuing_turn_attempt_id =
                                               attempt.issuing_turn_attempt_id
                                           AND child_wait.state_kind = 'terminal'
                                           AND child_wait.terminal_disposition_kind =
                                               'awaiting_child'
                                    )
                               )
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
            WHEN 'awaiting_runner_recovery' THEN
                IF live_attempt_count <> 0 THEN
                    RAISE EXCEPTION
                        'runner recovery wait retains a live turn attempt'
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
                    'tool_execution_result', 'tool_denied', 'tool_closed_by_turn_end', 'delegation_result'
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
                            'tool_execution_result', 'tool_denied', 'tool_closed_by_turn_end', 'delegation_result'
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
                        'tool_execution_result', 'tool_denied', 'tool_closed_by_turn_end', 'delegation_result'
                   )
           );
        IF unresolved_result_count <> 0 THEN
            RAISE EXCEPTION 'terminal tool-loop turn has unresolved requests'
                USING ERRCODE = '23514';
        END IF;
    END IF;
END;
$$;


--
-- Name: assert_tool_request_single_result(uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION assert_tool_request_single_result(checked_request_id uuid) RETURNS void
    LANGUAGE plpgsql
    AS $$
DECLARE
    result_count bigint;
BEGIN
    IF checked_request_id IS NULL THEN
        RETURN;
    END IF;
    SELECT count(*)
      INTO result_count
      FROM semantic_transcript_entry AS entry
      LEFT JOIN tool_attempt AS attempt
        ON attempt.attempt_id = entry.tool_result_attempt_id
     WHERE entry.payload_kind IN (
            'tool_execution_result', 'tool_denied', 'tool_closed_by_turn_end', 'delegation_result'
       )
       AND (
            entry.tool_result_request_id = checked_request_id
            OR attempt.request_id = checked_request_id
       );
    IF result_count > 1 THEN
        RAISE EXCEPTION
            'tool request % has more than one logical result',
            checked_request_id
            USING ERRCODE = '23505';
    END IF;
END;
$$;


--
-- Name: assert_tool_round_final_state(uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION assert_tool_round_final_state(checked_model_call_id uuid) RETURNS void
    LANGUAGE plpgsql
    AS $$
DECLARE
    round_record tool_round%ROWTYPE;
    source_frontier uuid;
    source_count numeric(20, 0);
    boundary_count numeric(20, 0);
    request_count bigint;
    assistant_part_count bigint;
    tool_use_count bigint;
    prefix_mismatch_count bigint;
    closed_result_count bigint;
BEGIN
    IF NOT claim_deferred_final_state_validation('tool_round', checked_model_call_id) THEN
        RETURN;
    END IF;

    SELECT *
      INTO round_record
      FROM tool_round
     WHERE producing_model_call_id = checked_model_call_id;
    IF NOT FOUND THEN
        RETURN;
    END IF;

    SELECT call.context_frontier_id
      INTO source_frontier
      FROM model_call AS call
      JOIN turn_attempt AS attempt
        ON attempt.turn_attempt_id = call.turn_attempt_id
       AND attempt.turn_id = call.turn_id
       AND attempt.session_id = call.session_id
     WHERE call.model_call_id = checked_model_call_id
       AND call.turn_id = round_record.turn_id
       AND call.session_id = round_record.session_id
       AND call.state_kind = 'terminal'
       AND call.terminal_disposition_kind = 'completed'
       AND (
            (
                round_record.boundary_kind = 'continuing'
                AND attempt.state_kind = 'ended'
                AND attempt.end_disposition = 'yielded_to_durable_wait'
            )
            OR (
                round_record.boundary_kind = 'closed_by_turn_end'
                AND attempt.state_kind = 'ended'
                AND attempt.end_variant = 'after_cancellation'
                AND attempt.end_disposition = 'cancelled'
            )
       );
    IF NOT FOUND THEN
        RAISE EXCEPTION 'tool round lacks its completed producing call'
            USING ERRCODE = '23514';
    END IF;

    SELECT count(*)
      INTO request_count
      FROM tool_request
     WHERE producing_model_call_id = checked_model_call_id;
    IF request_count <> round_record.request_count
       OR EXISTS (
            SELECT 1
              FROM generate_series(
                    0,
                    round_record.request_count::bigint - 1
              ) AS expected(request_ordinal)
              LEFT JOIN tool_request AS request
                ON request.producing_model_call_id = checked_model_call_id
               AND request.request_ordinal = expected.request_ordinal
             WHERE request.request_id IS NULL
       )
    THEN
        RAISE EXCEPTION 'tool round request inventory is not gapless'
            USING ERRCODE = '23514';
    END IF;

    SELECT count(*)
      INTO assistant_part_count
      FROM semantic_transcript_entry
     WHERE source_session_id = round_record.session_id
       AND producing_model_call_id = checked_model_call_id
       AND payload_kind IN ('assistant_text', 'assistant_tool_use');
    SELECT count(*)
      INTO tool_use_count
      FROM semantic_transcript_entry
     WHERE source_session_id = round_record.session_id
       AND producing_model_call_id = checked_model_call_id
       AND payload_kind = 'assistant_tool_use';
    IF assistant_part_count <> round_record.response_part_count
       OR tool_use_count <> round_record.request_count
    THEN
        RAISE EXCEPTION 'tool round lacks its exact assistant entry inventory'
            USING ERRCODE = '23514';
    END IF;

    SELECT member_count
      INTO source_count
      FROM context_frontier
     WHERE owning_session_id = round_record.session_id
       AND context_frontier_id = source_frontier;
    SELECT member_count
      INTO boundary_count
      FROM context_frontier
     WHERE owning_session_id = round_record.session_id
       AND context_frontier_id = round_record.boundary_frontier_id;
    SELECT count(*)
      INTO prefix_mismatch_count
      FROM context_frontier_member AS source_member
      LEFT JOIN context_frontier_member AS boundary_member
        ON boundary_member.owning_session_id = source_member.owning_session_id
       AND boundary_member.context_frontier_id =
           round_record.boundary_frontier_id
       AND boundary_member.member_position = source_member.member_position
       AND boundary_member.source_session_id = source_member.source_session_id
       AND boundary_member.semantic_entry_id = source_member.semantic_entry_id
     WHERE source_member.owning_session_id = round_record.session_id
       AND source_member.context_frontier_id = source_frontier
       AND boundary_member.member_position IS NULL;

    IF prefix_mismatch_count <> 0
       OR boundary_count < source_count + round_record.response_part_count
       OR EXISTS (
            SELECT 1
              FROM generate_series(
                    0,
                    round_record.response_part_count::bigint - 1
              ) AS expected(response_part_ordinal)
              LEFT JOIN semantic_transcript_entry AS entry
                ON entry.source_session_id = round_record.session_id
               AND entry.producing_model_call_id = checked_model_call_id
               AND entry.payload_kind IN (
                    'assistant_text',
                    'assistant_tool_use'
               )
               AND entry.assistant_response_part_ordinal =
                   expected.response_part_ordinal
              LEFT JOIN context_frontier_member AS member
                ON member.owning_session_id = round_record.session_id
               AND member.context_frontier_id =
                   round_record.boundary_frontier_id
               AND member.member_position =
                   source_count + expected.response_part_ordinal + 1
               AND member.source_session_id = entry.source_session_id
               AND member.semantic_entry_id = entry.semantic_entry_id
             WHERE entry.semantic_entry_id IS NULL
                OR member.member_position IS NULL
       )
       OR EXISTS (
            SELECT 1
              FROM (
                    SELECT
                        request.request_ordinal,
                        row_number() OVER (
                            ORDER BY member.member_position
                        ) - 1 AS frontier_request_ordinal
                      FROM tool_request AS request
                      JOIN semantic_transcript_entry AS entry
                        ON entry.source_session_id = round_record.session_id
                       AND entry.producing_model_call_id =
                           checked_model_call_id
                       AND entry.payload_kind = 'assistant_tool_use'
                       AND entry.assistant_tool_request_id =
                           request.request_id
                      JOIN context_frontier_member AS member
                        ON member.owning_session_id =
                           round_record.session_id
                       AND member.context_frontier_id =
                           round_record.boundary_frontier_id
                       AND member.source_session_id =
                           entry.source_session_id
                       AND member.semantic_entry_id =
                           entry.semantic_entry_id
                     WHERE request.producing_model_call_id =
                           checked_model_call_id
              ) AS ordered_request
             WHERE ordered_request.request_ordinal
                   <> ordered_request.frontier_request_ordinal
       )
    THEN
        RAISE EXCEPTION 'tool round frontier omits its ordered response'
            USING ERRCODE = '23514';
    END IF;

    IF round_record.boundary_kind = 'continuing' THEN
        IF boundary_count
               IS DISTINCT FROM source_count + round_record.response_part_count
        THEN
            RAISE EXCEPTION 'continuing tool round boundary has extra content'
                USING ERRCODE = '23514';
        END IF;
    ELSE
        SELECT count(*)
          INTO closed_result_count
          FROM semantic_transcript_entry AS entry
          JOIN tool_request AS request
            ON request.request_id = entry.tool_result_request_id
         WHERE request.producing_model_call_id = checked_model_call_id
           AND entry.payload_kind = 'tool_closed_by_turn_end';
        IF closed_result_count <> round_record.request_count
           OR boundary_count IS DISTINCT FROM (
                source_count
                + round_record.response_part_count
                + round_record.request_count
                + 1
           )
           OR EXISTS (
                SELECT 1
                  FROM tool_request AS request
                  LEFT JOIN semantic_transcript_entry AS entry
                    ON entry.source_session_id = round_record.session_id
                   AND entry.payload_kind = 'tool_closed_by_turn_end'
                   AND entry.tool_result_request_id = request.request_id
                  LEFT JOIN context_frontier_member AS member
                    ON member.owning_session_id = round_record.session_id
                   AND member.context_frontier_id =
                       round_record.boundary_frontier_id
                   AND member.member_position = (
                        source_count
                        + round_record.response_part_count
                        + request.request_ordinal
                        + 1
                   )
                   AND member.source_session_id = entry.source_session_id
                   AND member.semantic_entry_id = entry.semantic_entry_id
                 WHERE request.producing_model_call_id =
                       checked_model_call_id
                   AND member.semantic_entry_id IS NULL
           )
           OR NOT EXISTS (
                SELECT 1
                  FROM semantic_transcript_entry AS entry
                  JOIN context_frontier_member AS member
                    ON member.owning_session_id = round_record.session_id
                   AND member.context_frontier_id =
                       round_record.boundary_frontier_id
                   AND member.member_position = boundary_count
                   AND member.source_session_id = entry.source_session_id
                   AND member.semantic_entry_id = entry.semantic_entry_id
                 WHERE entry.source_session_id = round_record.session_id
                   AND entry.payload_kind = 'turn_cancelled'
                   AND entry.cancelled_turn_id = round_record.turn_id
           )
           OR NOT EXISTS (
                SELECT 1
                  FROM turn_lifecycle
                 WHERE turn_id = round_record.turn_id
                   AND session_id = round_record.session_id
                   AND state_kind = 'terminal'
                   AND terminal_disposition_kind = 'cancelled'
                   AND terminal_frontier_id =
                       round_record.boundary_frontier_id
           )
        THEN
            RAISE EXCEPTION 'closed tool round lacks exact turn-end resolution'
                USING ERRCODE = '23514';
        END IF;
    END IF;
END;
$$;


--
-- Name: canonical_tool_json(text); Type: FUNCTION; Schema: public
--

CREATE FUNCTION canonical_tool_json(value_text text) RETURNS text
    LANGUAGE plpgsql IMMUTABLE STRICT
    AS $_$
DECLARE
    checked_value json;
    character_index integer := 1;
    character_count integer := char_length(value_text);
    current_character text;
    string_start integer;
    string_escape boolean := false;
    escape_code text;
    malformed_node boolean;
BEGIN
    checked_value := value_text::json;

    -- Validate compact canonical string spelling iteratively. The JSON parser
    -- establishes grammar; this scan rejects insignificant whitespace and
    -- escape spellings that serde_json's serializer would rewrite.
    WHILE character_index <= character_count LOOP
        current_character := substr(value_text, character_index, 1);
        IF string_start IS NULL THEN
            IF current_character ~ '[[:space:]]' THEN
                RETURN NULL;
            ELSIF current_character = '"' THEN
                string_start := character_index;
                string_escape := false;
            END IF;
        ELSIF string_escape THEN
            IF current_character = 'u' THEN
                escape_code := substr(value_text, character_index + 1, 4);
                IF escape_code !~ '^00(0[0-9a-f]|1[0-9a-f])$'
                   OR escape_code IN (
                        '0008',
                        '0009',
                        '000a',
                        '000c',
                        '000d'
                   )
                THEN
                    RETURN NULL;
                END IF;
            ELSIF current_character NOT IN ('"', chr(92), 'b', 'f', 'n', 'r', 't') THEN
                RETURN NULL;
            END IF;
            string_escape := false;
        ELSIF current_character = chr(92) THEN
            string_escape := true;
        ELSIF current_character = '"' THEN
            string_start := NULL;
        END IF;
        character_index := character_index + 1;
    END LOOP;

    -- Walk the parsed tree through PostgreSQL's iterative recursive-CTE
    -- executor. Each object must already have distinct C-lexical keys, and
    -- every number must have the same spelling as the domain canonicalizer.
    WITH RECURSIVE json_node(node_value) AS (
        SELECT checked_value
        UNION ALL
        SELECT child.node_value
          FROM json_node AS parent
          CROSS JOIN LATERAL (
                SELECT element AS node_value
                  FROM json_array_elements(
                        CASE json_typeof(parent.node_value)
                            WHEN 'array' THEN parent.node_value
                            ELSE '[]'::json
                        END
                  ) AS array_member(element)
                UNION ALL
                SELECT member_value AS node_value
                  FROM json_each(
                        CASE json_typeof(parent.node_value)
                            WHEN 'object' THEN parent.node_value
                            ELSE '{}'::json
                        END
                  ) AS object_member(member_key, member_value)
          ) AS child
    )
    SELECT EXISTS (
        SELECT 1
          FROM json_node
         WHERE (
                json_typeof(node_value) = 'number'
                AND canonical_tool_json_number(node_value::text)
                    IS DISTINCT FROM node_value::text
           )
            OR (
                json_typeof(node_value) = 'object'
                AND EXISTS (
                    SELECT 1
                      FROM json_each(node_value) WITH ORDINALITY
                           AS member(
                               member_key,
                               member_value,
                               member_position
                           )
                    HAVING count(*) <>
                               count(DISTINCT member_key COLLATE "C")
                        OR array_agg(member_key ORDER BY member_position)
                           IS DISTINCT FROM
                           array_agg(member_key ORDER BY member_key COLLATE "C")
                )
           )
    )
      INTO malformed_node;

    IF malformed_node THEN
        RETURN NULL;
    END IF;
    RETURN value_text;
EXCEPTION
    WHEN invalid_text_representation THEN
        RETURN NULL;
    WHEN untranslatable_character THEN
        -- PostgreSQL text cannot materialize an escaped U+0000 while walking
        -- JSON string values. The lexical scan above has already established
        -- serde_json's canonical escape spelling, so retain that valid JSON
        -- instead of misclassifying it as undecodable.
        RETURN value_text;
END;
$_$;


--
-- Name: canonical_tool_json_number(text); Type: FUNCTION; Schema: public
--

CREATE FUNCTION canonical_tool_json_number(value_text text) RETURNS text
    LANGUAGE plpgsql IMMUTABLE STRICT
    AS $$
DECLARE
    exponent_at integer;
    exponent_text text;
BEGIN
    IF value_text = '-0' THEN
        RETURN '0';
    END IF;
    exponent_at := strpos(lower(value_text), 'e');
    IF exponent_at = 0 THEN
        RETURN value_text;
    END IF;
    exponent_text := substr(value_text, exponent_at + 1);
    IF left(exponent_text, 1) NOT IN ('+', '-') THEN
        exponent_text := '+' || exponent_text;
    END IF;
    RETURN substr(value_text, 1, exponent_at - 1)
        || 'e'
        || exponent_text;
END;
$$;


--
-- Name: default_v1_queued_tool_auto_approval(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION default_v1_queued_tool_auto_approval() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NEW.source_configuration_turn_id IS NULL
       AND NEW.dangerous_tool_auto_approval IS NULL
    THEN
        NEW.dangerous_tool_auto_approval := 'disabled';
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: evaluation_corpus_path_components_bounded(text); Type: FUNCTION; Schema: public
--

CREATE FUNCTION evaluation_corpus_path_components_bounded(source_path text) RETURNS boolean
    LANGUAGE sql IMMUTABLE STRICT
    RETURN (SELECT bool_and((octet_length(component.component) <= 255)) AS bool_and FROM unnest(string_to_array(evaluation_corpus_path_components_bounded.source_path, '/'::text)) component(component));


--
-- Name: evaluation_corpus_text_is_nonblank_control_free(text); Type: FUNCTION; Schema: public
--

CREATE FUNCTION evaluation_corpus_text_is_nonblank_control_free(value text) RETURNS boolean
    LANGUAGE sql IMMUTABLE STRICT
    RETURN ((NOT (EXISTS (SELECT 1 FROM unnest(string_to_array(evaluation_corpus_text_is_nonblank_control_free.value, NULL::text)) "character"("character") WHERE (((ascii("character"."character") >= 0) AND (ascii("character"."character") <= 31)) OR ((ascii("character"."character") >= 127) AND (ascii("character"."character") <= 159)))))) AND (EXISTS (SELECT 1 FROM unnest(string_to_array(evaluation_corpus_text_is_nonblank_control_free.value, NULL::text)) "character"("character") WHERE ((ascii("character"."character") <> ALL (ARRAY[9, 10, 11, 12, 13, 32, 133, 160, 5760, 8232, 8233, 8239, 8287, 12288])) AND ((ascii("character"."character") < 8192) OR (ascii("character"."character") > 8202))))));


--
-- Name: project_terminal_approval_judge_usage(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION project_terminal_approval_judge_usage() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    INSERT INTO web_usage_call_projection (
        model_call_id, call_kind, session_id, turn_id,
        resolved_provider_model_identity_id, credential_profile_label,
        usage_provenance_kind, usage_input_includes_cache_tokens,
        input_tokens, output_tokens,
        cache_creation_input_tokens, cache_read_input_tokens
    ) VALUES (
        NEW.model_call_id, 'approval_judge', NEW.session_id, NEW.turn_id,
        NEW.resolved_provider_model_identity_id,
        bounded_web_usage_profile(NEW.credential_reference),
        NEW.usage_provenance_kind, NEW.usage_input_includes_cache_tokens,
        NEW.input_tokens, NEW.output_tokens,
        NEW.cache_creation_input_tokens, NEW.cache_read_input_tokens
    );
    RETURN NEW;
END;
$$;


--
-- Name: reject_eval_call_outside_run_recording(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_eval_call_outside_run_recording() RETURNS trigger
    LANGUAGE plpgsql
    AS $_$
DECLARE
    run_transaction xid8;
BEGIN
    EXECUTE pg_catalog.format(
        'SELECT recording_transaction_id '
        'FROM %I.approval_judge_eval_run WHERE eval_run_id = $1',
        TG_TABLE_SCHEMA
    )
    INTO run_transaction
    USING NEW.eval_run_id;
    IF run_transaction IS NULL THEN
        RAISE EXCEPTION 'approval_judge_eval_call requires its run row first'
            USING ERRCODE = '23514';
    END IF;
    IF run_transaction <> pg_current_xact_id() THEN
        RAISE EXCEPTION 'approval_judge_eval_call is sealed with its run; '
            'rows admit insertion only in the recording transaction'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$_$;


--
-- Name: reject_tool_approval_judge_call_invalid_change(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_tool_approval_judge_call_invalid_change() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    active_wait boolean;
    request_posture text;
BEGIN
    IF TG_OP = 'INSERT' THEN
        SELECT approval_posture INTO request_posture
          FROM tool_request
         WHERE request_id = NEW.request_id
           FOR UPDATE;
        SELECT true INTO active_wait
          FROM turn_lifecycle AS lifecycle
         WHERE lifecycle.turn_id = NEW.turn_id
           AND lifecycle.session_id = NEW.session_id
           AND lifecycle.state_kind = 'active'
           AND lifecycle.active_phase_kind = 'awaiting_tool_approval'
           AND lifecycle.approval_tool_request_id = NEW.request_id
           FOR UPDATE;
        IF active_wait IS DISTINCT FROM true OR EXISTS (
            SELECT 1
              FROM tool_approval_decision AS decision
             WHERE decision.request_id = NEW.request_id
        ) THEN
            RAISE EXCEPTION 'approval judge call lacks an active approval wait'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'tool_approval_judge_requires_active_wait';
        END IF;
        IF NEW.usage_provenance_kind <> 'reported' THEN
            RAISE EXCEPTION 'prepared approval judge usage must be reported'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'tool_approval_judge_prepared_usage_is_reported';
        END IF;
        IF NEW.state_kind <> 'prepared'
            OR NEW.terminal_disposition_kind IS NOT NULL
            OR NEW.recommendation_kind IS NOT NULL
            OR NEW.rationale IS NOT NULL
            OR NEW.input_tokens IS NOT NULL
            OR NEW.output_tokens IS NOT NULL
            OR NEW.cache_read_input_tokens IS NOT NULL
            OR NEW.cache_creation_input_tokens IS NOT NULL
        THEN
            RAISE EXCEPTION 'approval judge call must be inserted as Prepared'
                USING ERRCODE = '23514';
        END IF;
        RETURN NEW;
    END IF;
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'approval judge call is not deletable'
            USING ERRCODE = '23514';
    END IF;
    IF ROW(
        OLD.model_call_id, OLD.request_id, OLD.session_id, OLD.turn_id,
        OLD.direct_model_selection_id,
        OLD.resolved_provider_model_identity_id,
        OLD.credential_reference, OLD.usage_input_includes_cache_tokens
    ) IS DISTINCT FROM ROW(
        NEW.model_call_id, NEW.request_id, NEW.session_id, NEW.turn_id,
        NEW.direct_model_selection_id,
        NEW.resolved_provider_model_identity_id,
        NEW.credential_reference, NEW.usage_input_includes_cache_tokens
    ) OR (
        NEW.usage_provenance_kind IS DISTINCT FROM OLD.usage_provenance_kind
        AND NOT (
            OLD.state_kind <> 'terminal'
            AND NEW.state_kind = 'terminal'
        )
    ) THEN
        RAISE EXCEPTION 'approval judge authorization facts are immutable'
            USING ERRCODE = '23514';
    END IF;
    IF OLD.state_kind = 'terminal' THEN
        RAISE EXCEPTION 'terminal approval judge call is immutable'
            USING ERRCODE = '23514';
    END IF;
    IF OLD.state_kind = 'prepared'
       AND NEW.state_kind = 'terminal'
       AND NEW.terminal_disposition_kind NOT IN ('known_failed', 'cancelled')
    THEN
        RAISE EXCEPTION 'prepared approval judge cannot record provider outcome'
            USING ERRCODE = '23514';
    END IF;
    IF OLD.state_kind = 'prepared'
       AND NEW.state_kind = 'terminal'
       AND (
            NEW.input_tokens IS NOT NULL
            OR NEW.output_tokens IS NOT NULL
            OR NEW.cache_read_input_tokens IS NOT NULL
            OR NEW.cache_creation_input_tokens IS NOT NULL
       )
    THEN
        RAISE EXCEPTION 'unsent approval judge cannot record provider usage'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'tool_approval_judge_unsent_has_no_usage';
    END IF;
    IF NOT (
        (OLD.state_kind = 'prepared' AND NEW.state_kind IN ('in_flight', 'terminal'))
        OR (OLD.state_kind = 'in_flight' AND NEW.state_kind = 'terminal')
    ) THEN
        RAISE EXCEPTION 'invalid approval judge call transition'
            USING ERRCODE = '23514';
    END IF;
    IF NEW.state_kind <> 'terminal' AND (
        NEW.input_tokens IS NOT NULL
        OR NEW.output_tokens IS NOT NULL
        OR NEW.cache_read_input_tokens IS NOT NULL
        OR NEW.cache_creation_input_tokens IS NOT NULL
    ) THEN
        RAISE EXCEPTION 'approval judge usage is terminal evidence'
            USING ERRCODE = '23514';
    END IF;
    IF NEW.state_kind = 'terminal'
       AND NEW.terminal_disposition_kind = 'completed'
    THEN
        SELECT approval_posture INTO request_posture
          FROM tool_request WHERE request_id = NEW.request_id;
        IF request_posture = 'auto'
           OR (request_posture = 'human'
               AND NEW.recommendation_kind <> 'escalate_to_human')
        THEN
            RAISE EXCEPTION 'approval judge recommendation exceeds frozen posture'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'tool_approval_judge_recommendation_within_posture';
        END IF;
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: reject_tool_attempt_invalid_change(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_tool_attempt_invalid_change() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        IF NEW.state_kind <> 'prepared' THEN
            RAISE EXCEPTION 'tool attempt must be inserted as Prepared'
                USING
                    ERRCODE = '23514',
                    CONSTRAINT = 'tool_attempt_inserted_prepared';
        END IF;
        RETURN NEW;
    END IF;

    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'tool_attempt is not deletable'
            USING ERRCODE = '23514';
    END IF;

    IF ROW(
        OLD.attempt_id,
        OLD.request_id,
        OLD.session_id,
        OLD.turn_id,
        OLD.issuing_turn_attempt_id,
        OLD.effect_class,
        OLD.dispatch_generation
    ) IS DISTINCT FROM ROW(
        NEW.attempt_id,
        NEW.request_id,
        NEW.session_id,
        NEW.turn_id,
        NEW.issuing_turn_attempt_id,
        NEW.effect_class,
        NEW.dispatch_generation
    ) THEN
        RAISE EXCEPTION 'tool attempt authorization facts are immutable'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.state_kind = 'terminal' THEN
        RAISE EXCEPTION 'terminal tool attempt is immutable'
            USING ERRCODE = '23514';
    END IF;

    IF NOT (
        OLD.state_kind = NEW.state_kind
        OR (
            OLD.state_kind = 'prepared'
            AND NEW.state_kind IN ('in_flight', 'terminal')
        )
        OR (
            OLD.state_kind = 'in_flight'
            AND NEW.state_kind = 'terminal'
        )
    ) THEN
        RAISE EXCEPTION 'tool attempt transition is not monotonic'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.state_kind = 'prepared'
       AND NEW.state_kind = 'terminal'
       AND (
            NEW.terminal_disposition_kind <> 'known_failed'
            OR NEW.error_kind NOT IN (
                'unknown_tool',
                'invalid_arguments',
                'preauthorization_rejected',
                'crash_lost'
            )
       )
    THEN
        RAISE EXCEPTION 'unsent tool attempt has impossible terminal evidence'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.state_kind = 'in_flight'
       AND NEW.state_kind = 'terminal'
       AND (
            NEW.error_kind IN (
                'unknown_tool',
                'invalid_arguments',
                'preauthorization_rejected'
            )
            OR (
                OLD.effect_class = 'external_effect'
                AND NEW.error_kind = 'crash_lost'
                -- Carried unchanged from
                -- 202608080101_runner_recovery_turn_phase.sql: only the exact
                -- current lineage — the active awaiting-recovery phase bound to
                -- this attempt, its bound lease, and that lease's current
                -- lost-unclaimed head — admits crash loss for a dispatched
                -- external effect. An attempt_id match alone would accept a
                -- stale proof from an earlier lease generation.
                AND NOT EXISTS (
                    SELECT 1
                      FROM turn_lifecycle AS lifecycle
                      JOIN runner_physical_attempt_lease_binding AS binding
                        ON binding.attempt_id = OLD.attempt_id
                      JOIN runner_lease_generation AS lease
                        ON lease.lease_id = binding.lease_id
                       AND lease.attempt_id = OLD.attempt_id
                       AND lease.session_id = OLD.session_id
                      JOIN runner_current_lease_event AS lease_head
                        ON lease_head.lease_id = lease.lease_id
                       AND lease_head.generation = lease.generation
                      JOIN runner_lease_event AS lease_event
                        ON lease_event.lease_id = lease_head.lease_id
                       AND lease_event.generation = lease_head.generation
                       AND lease_event.event_ordinal = lease_head.event_ordinal
                     WHERE lifecycle.session_id = OLD.session_id
                       AND lifecycle.turn_id = OLD.turn_id
                       AND lifecycle.state_kind = 'active'
                       AND lifecycle.active_phase_kind =
                            'awaiting_runner_recovery'
                       AND lifecycle.runner_recovery_tool_attempt_id =
                            OLD.attempt_id
                       AND lease_event.state_kind = 'lost_unclaimed'
                )
            )
       )
    THEN
        RAISE EXCEPTION 'dispatched tool attempt has impossible terminal evidence'
            USING ERRCODE = '23514';
    END IF;

    RETURN NEW;
END;
$$;


--
-- Name: reject_tool_continuation_context_headroom_truncate(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_tool_continuation_context_headroom_truncate() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    RAISE EXCEPTION '% cannot be truncated', TG_TABLE_NAME
        USING ERRCODE = '23514';
END;
$$;


--
-- Name: require_completed_tool_approval_judge_decision(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_completed_tool_approval_judge_decision() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    matching_decisions bigint;
BEGIN
    IF NEW.state_kind <> 'terminal'
       OR NEW.terminal_disposition_kind <> 'completed'
       OR NEW.recommendation_kind = 'escalate_to_human'
    THEN
        RETURN NULL;
    END IF;

    SELECT count(*)
      INTO matching_decisions
      FROM tool_approval_decision AS decision
     WHERE decision.request_id = NEW.request_id
       AND decision.decision_source = 'delegate'
       AND decision.decision_kind = NEW.recommendation_kind
       AND decision.delegate_model_selection_id =
           NEW.direct_model_selection_id
       AND decision.delegate_model_call_id = NEW.model_call_id
       AND decision.rationale = NEW.rationale;
    IF matching_decisions <> 1 THEN
        RAISE EXCEPTION
            'completed approval judge lacks its exact decision'
            USING
                ERRCODE = '23514',
                CONSTRAINT =
                    'tool_approval_judge_completed_requires_decision_effect';
    END IF;
    RETURN NULL;
END;
$$;


--
-- Name: require_denied_tool_without_attempt(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_denied_tool_without_attempt() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NEW.decision_kind = 'deny'
       AND EXISTS (
            SELECT 1
              FROM tool_attempt
             WHERE request_id = NEW.request_id
       )
    THEN
        RAISE EXCEPTION 'denied tool request cannot have an attempt'
            USING
                ERRCODE = '23514',
                CONSTRAINT = 'denied_tool_request_has_no_attempt';
    END IF;
    RETURN NULL;
END;
$$;


--
-- Name: require_explicit_tool_approval_decided_outbox(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_explicit_tool_approval_decided_outbox() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NEW.recording_transaction_id <> pg_current_xact_id() THEN
        RAISE EXCEPTION 'tool approval decided event transaction is not current'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'tool_approval_decided_transaction_current';
    END IF;
    IF NOT EXISTS (
        SELECT 1 FROM tool_approval_decision
         WHERE request_id = NEW.request_id
           AND decision_source NOT IN ('policy_auto', 'session_blanket')
    ) THEN
        RAISE EXCEPTION 'tool approval decided event requires explicit provenance'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'tool_approval_decided_requires_explicit_source';
    END IF;
    RETURN NULL;
END;
$$;


--
-- Name: require_explicit_tool_approval_effect(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_explicit_tool_approval_effect() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    matching_effects bigint;
BEGIN
    IF NEW.decision_source IN (
        'policy_auto', 'session_blanket', 'runtime_safety'
    ) THEN
        RETURN NULL;
    END IF;

    IF NEW.decision_source = 'user_override' THEN
        SELECT count(*)
          INTO matching_effects
          FROM tool_approval_decided_outbox_event AS dispatched
          JOIN tool_request AS request
            ON request.request_id = dispatched.request_id
          JOIN turn_lifecycle AS lifecycle
            ON lifecycle.turn_id = request.turn_id
           AND lifecycle.session_id = request.session_id
         WHERE dispatched.request_id = NEW.request_id
           AND dispatched.recording_transaction_id = pg_current_xact_id()
           AND lifecycle.active_tool_round_call_id =
               request.producing_model_call_id
           AND (
                (
                    lifecycle.state_kind = 'active'
                    AND lifecycle.active_phase_kind = 'awaiting_tool_approval'
                    AND lifecycle.approval_tool_request_id = (
                        SELECT later.request_id
                          FROM tool_request AS later
                          LEFT JOIN tool_approval_decision AS later_decision
                            ON later_decision.request_id = later.request_id
                         WHERE later.producing_model_call_id =
                               request.producing_model_call_id
                           AND later_decision.request_id IS NULL
                         ORDER BY later.request_ordinal
                         LIMIT 1
                    )
                )
                OR
                (
                    lifecycle.state_kind = 'active'
                    AND lifecycle.active_phase_kind = 'running'
                    AND lifecycle.approval_tool_request_id IS NULL
                    AND lifecycle.recovery_tool_attempt_id IS NULL
                    AND NOT EXISTS (
                        SELECT 1
                          FROM tool_request AS undecided
                          LEFT JOIN tool_approval_decision AS undecided_decision
                            ON undecided_decision.request_id =
                               undecided.request_id
                         WHERE undecided.producing_model_call_id =
                               request.producing_model_call_id
                           AND undecided_decision.request_id IS NULL
                    )
                    AND EXISTS (
                        SELECT 1
                          FROM turn_attempt AS successor
                          JOIN model_call AS producing_call
                            ON producing_call.model_call_id =
                               request.producing_model_call_id
                           AND producing_call.turn_id = request.turn_id
                           AND producing_call.session_id = request.session_id
                         WHERE successor.turn_attempt_id =
                               lifecycle.current_attempt_id
                           AND successor.turn_id = request.turn_id
                           AND successor.session_id = request.session_id
                           AND successor.continued_from_attempt_id =
                               producing_call.turn_attempt_id
                           AND successor.state_kind = 'prepared'
                    )
                )
           );
        IF matching_effects <> 1 THEN
            RAISE EXCEPTION
                'user override consumption lacks its atomic proposal effect'
                USING
                    ERRCODE = '23514',
                    CONSTRAINT =
                        'tool_approval_user_override_requires_atomic_effect';
        END IF;
        RETURN NULL;
    END IF;

    SELECT count(*)
      INTO matching_effects
      FROM tool_approval_decided_outbox_event AS dispatched
      JOIN tool_request AS request
        ON request.request_id = dispatched.request_id
      JOIN turn_lifecycle AS lifecycle
        ON lifecycle.turn_id = request.turn_id
       AND lifecycle.session_id = request.session_id
     WHERE dispatched.request_id = NEW.request_id
       AND dispatched.recording_transaction_id = pg_current_xact_id()
       AND (
            lifecycle.active_tool_round_call_id =
                request.producing_model_call_id
            OR lifecycle.state_kind = 'terminal'
       )
       AND ((
            SELECT count(*)
              FROM tool_approval_decided_outbox_event AS transaction_event
              JOIN tool_approval_decision AS transaction_decision
                ON transaction_decision.request_id =
                   transaction_event.request_id
              JOIN tool_request AS transaction_request
                ON transaction_request.request_id =
                   transaction_decision.request_id
             WHERE transaction_request.producing_model_call_id =
                   request.producing_model_call_id
               AND transaction_decision.decision_source NOT IN (
                    'policy_auto', 'session_blanket', 'runtime_safety'
               )
               AND transaction_event.recording_transaction_id =
                   dispatched.recording_transaction_id
       ) = 1 OR lifecycle.state_kind = 'terminal')
       AND NOT EXISTS (
            SELECT 1
              FROM tool_request AS earlier
              LEFT JOIN tool_approval_decision AS earlier_decision
                ON earlier_decision.request_id = earlier.request_id
             WHERE earlier.producing_model_call_id =
                   request.producing_model_call_id
               AND earlier.request_ordinal < request.request_ordinal
               AND earlier_decision.request_id IS NULL
       )
       AND (
            (
                lifecycle.state_kind = 'active'
                AND lifecycle.active_phase_kind = 'awaiting_tool_approval'
                AND lifecycle.approval_tool_request_id = (
                    SELECT later.request_id
                      FROM tool_request AS later
                      LEFT JOIN tool_approval_decision AS later_decision
                        ON later_decision.request_id = later.request_id
                     WHERE later.producing_model_call_id =
                           request.producing_model_call_id
                       AND later_decision.request_id IS NULL
                     ORDER BY later.request_ordinal
                     LIMIT 1
                )
            )
            OR
            (
                lifecycle.state_kind = 'active'
                AND lifecycle.active_phase_kind = 'running'
                AND lifecycle.approval_tool_request_id IS NULL
                AND lifecycle.recovery_tool_attempt_id IS NULL
                AND NOT EXISTS (
                    SELECT 1
                      FROM tool_request AS undecided
                      LEFT JOIN tool_approval_decision AS undecided_decision
                        ON undecided_decision.request_id = undecided.request_id
                     WHERE undecided.producing_model_call_id =
                           request.producing_model_call_id
                       AND undecided_decision.request_id IS NULL
                )
                AND EXISTS (
                    SELECT 1
                      FROM turn_attempt AS successor
                      JOIN model_call AS producing_call
                        ON producing_call.model_call_id =
                           request.producing_model_call_id
                       AND producing_call.turn_id = request.turn_id
                       AND producing_call.session_id = request.session_id
                     WHERE successor.turn_attempt_id =
                           lifecycle.current_attempt_id
                       AND successor.turn_id = request.turn_id
                       AND successor.session_id = request.session_id
                       AND successor.continued_from_attempt_id =
                           producing_call.turn_attempt_id
                       AND successor.state_kind = 'prepared'
                )
            )
            OR
            (
                lifecycle.state_kind = 'terminal'
                AND NOT EXISTS (
                    SELECT 1
                      FROM tool_request AS undecided
                      LEFT JOIN tool_approval_decision AS undecided_decision
                        ON undecided_decision.request_id =
                           undecided.request_id
                     WHERE undecided.producing_model_call_id =
                           request.producing_model_call_id
                       AND undecided_decision.request_id IS NULL
                )
                AND EXISTS (
                    SELECT 1
                      FROM turn_attempt AS stopped_attempt
                      JOIN submit_input_command AS interrupt
                        ON interrupt.command_id =
                           stopped_attempt.interrupt_command_id
                      JOIN durable_command AS interrupt_claim
                        ON interrupt_claim.command_id = interrupt.command_id
                     WHERE stopped_attempt.turn_attempt_id =
                           lifecycle.terminal_attempt_id
                       AND interrupt.delivery_kind = 'interrupt'
                       AND interrupt.result_kind = 'applied'
                       AND interrupt_claim.issuer_kind = 'core'
                       AND interrupt_claim.claimed_at =
                           transaction_timestamp()
                )
            )
       );
    IF matching_effects <> 1 THEN
        RAISE EXCEPTION
            'explicit decision lacks its outbox and lifecycle effect'
            USING
                ERRCODE = '23514',
                CONSTRAINT =
                    'tool_approval_explicit_requires_atomic_effect';
    END IF;
    RETURN NULL;
END;
$$;


--
-- Name: require_override_command_effect(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_override_command_effect() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    override_count bigint;
BEGIN
    SELECT count(*) INTO override_count
      FROM tool_approval_user_override
     WHERE command_id = NEW.command_id;
    IF (NEW.result_kind = 'applied' AND override_count <> 1)
       OR (NEW.result_kind = 'rejected' AND override_count <> 0)
    THEN
        RAISE EXCEPTION 'override command lacks its exact recorded effect'
            USING ERRCODE = '23514',
                  CONSTRAINT =
                      'override_denied_tool_request_command_requires_effect';
    END IF;
    RETURN NULL;
END;
$$;


--
-- Name: require_session_blanket_approval_provenance(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_session_blanket_approval_provenance() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    checked_turn uuid;
    checked_session uuid;
    blanket_root_count bigint;
BEGIN
    IF NEW.decision_source <> 'session_blanket' THEN
        RETURN NULL;
    END IF;

    SELECT turn_id, session_id
      INTO checked_turn, checked_session
      FROM tool_request
     WHERE request_id = NEW.request_id;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'session blanket approval lacks its tool request'
            USING
                ERRCODE = '23514',
                CONSTRAINT =
                    'tool_approval_session_blanket_requires_frozen_approve_all';
    END IF;

    WITH RECURSIVE configuration_origin AS (
        SELECT stored.*
          FROM queued_input_origin AS stored
         WHERE stored.turn_id = checked_turn
           AND stored.session_id = checked_session
        UNION
        SELECT source.*
          FROM configuration_origin AS current
          JOIN queued_input_origin AS source
            ON source.turn_id = current.source_configuration_turn_id
           AND source.session_id = current.session_id
    )
    SELECT count(*)
      INTO blanket_root_count
      FROM configuration_origin
     WHERE source_configuration_turn_id IS NULL
       AND dangerous_tool_auto_approval = 'approve_all';

    IF blanket_root_count <> 1 THEN
        RAISE EXCEPTION
            'session blanket approval requires frozen approve-all authority'
            USING
                ERRCODE = '23514',
                CONSTRAINT =
                    'tool_approval_session_blanket_requires_frozen_approve_all';
    END IF;
    RETURN NULL;
END;
$$;


--
-- Name: require_single_tool_result(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_single_tool_result() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    old_request_id uuid;
    new_request_id uuid;
BEGIN
    IF TG_OP <> 'INSERT' THEN
        old_request_id := OLD.tool_result_request_id;
        IF old_request_id IS NULL AND OLD.tool_result_attempt_id IS NOT NULL THEN
            SELECT request_id
              INTO old_request_id
              FROM tool_attempt
             WHERE attempt_id = OLD.tool_result_attempt_id;
        END IF;
        PERFORM assert_tool_request_single_result(old_request_id);
    END IF;
    IF TG_OP <> 'DELETE' THEN
        new_request_id := NEW.tool_result_request_id;
        IF new_request_id IS NULL AND NEW.tool_result_attempt_id IS NOT NULL THEN
            SELECT request_id
              INTO new_request_id
              FROM tool_attempt
             WHERE attempt_id = NEW.tool_result_attempt_id;
        END IF;
        IF new_request_id IS DISTINCT FROM old_request_id THEN
            PERFORM assert_tool_request_single_result(new_request_id);
        END IF;
    END IF;
    RETURN NULL;
END;
$$;


--
-- Name: require_tool_approval_decision_authority(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_tool_approval_decision_authority() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    matched bigint;
BEGIN
    PERFORM 1
       FROM tool_request
      WHERE request_id = NEW.request_id
        FOR UPDATE;
    IF EXISTS (
        SELECT 1
          FROM tool_approval_judge_model_call AS judge
         WHERE judge.request_id = NEW.request_id
           AND judge.state_kind <> 'terminal'
    ) THEN
        RAISE EXCEPTION 'approval decision races an unfinished judge call'
            USING ERRCODE = '23514',
                  CONSTRAINT =
                      'tool_approval_decision_requires_terminal_judge';
    END IF;
    IF NEW.decision_source = 'runtime_safety' THEN
        SELECT count(*) INTO matched
          FROM tool_request
         WHERE request_id = NEW.request_id
           AND approval_posture = 'auto'
           AND arguments_kind = 'json'
           AND arguments_text = '{"redacted":"[redacted]"}';
        IF matched <> 1 THEN
            RAISE EXCEPTION 'runtime safety denial lacks suppressed arguments'
                USING ERRCODE = '23514',
                      CONSTRAINT =
                          'tool_approval_runtime_safety_requires_suppressed_arguments';
        END IF;
        RETURN NULL;
    END IF;
    IF NEW.decision_source IN ('policy_auto', 'session_blanket') THEN
        SELECT count(*) INTO matched
          FROM tool_request
         WHERE request_id = NEW.request_id
           AND approval_posture = 'auto';
        IF matched <> 1 THEN
            RAISE EXCEPTION 'automatic decision exceeds frozen posture'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'tool_approval_automatic_requires_auto_posture';
        END IF;
        RETURN NULL;
    END IF;
    IF NEW.decision_source = 'user_override' THEN
        SELECT count(*) INTO matched
          FROM tool_request AS request
          JOIN tool_approval_user_override AS recorded
            ON recorded.denied_request_id = NEW.override_denied_request_id
          JOIN model_call_user_override AS frozen
            ON frozen.model_call_id = request.producing_model_call_id
           AND frozen.denied_request_id = recorded.denied_request_id
          JOIN tool_request AS denied_request
            ON denied_request.request_id = recorded.denied_request_id
         WHERE request.request_id = NEW.request_id
           AND request.approval_posture = 'delegated'
           AND recorded.session_id = request.session_id
           AND denied_request.tool_name = request.tool_name
           AND denied_request.arguments_kind = request.arguments_kind
           AND denied_request.arguments_text = request.arguments_text;
        IF matched <> 1 THEN
            RAISE EXCEPTION
                'user override consumption lacks a recorded override for a delegated request'
                USING ERRCODE = '23514',
                      CONSTRAINT =
                          'tool_approval_user_override_requires_recorded_override';
        END IF;
        RETURN NULL;
    END IF;
    IF NEW.decision_source = 'user_command' THEN
        SELECT count(*) INTO matched
          FROM tool_request AS request
         WHERE request.request_id = NEW.request_id
           AND (
                request.approval_posture = 'human'
                OR (
                    request.approval_posture = 'delegated'
                    AND EXISTS (
                        SELECT 1
                          FROM tool_approval_judge_model_call AS judge
                         WHERE judge.request_id = request.request_id
                           AND judge.state_kind = 'terminal'
                           AND (
                                (
                                    judge.terminal_disposition_kind = 'completed'
                                    AND judge.recommendation_kind =
                                        'escalate_to_human'
                                )
                                OR judge.terminal_disposition_kind IN (
                                    'known_failed', 'refused', 'cancelled',
                                    'ambiguous'
                                )
                           )
                    )
                )
           );
        IF matched <> 1 THEN
            RAISE EXCEPTION 'user decision lacks human approval authority'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'tool_approval_user_requires_human_authority';
        END IF;
        RETURN NULL;
    END IF;
    IF NEW.decision_source = 'lifecycle_closure' THEN
        SELECT count(*) INTO matched
          FROM decide_tool_request_command AS command
          JOIN durable_command AS durable
            ON durable.command_id = command.command_id
           AND durable.command_kind = command.command_kind
           AND durable.storage_version = command.storage_version
         WHERE command.command_id = NEW.user_command_id
           AND command.request_id = NEW.request_id
           AND command.decision_kind = 'deny'
           AND command.denial_reason IS NULL
           AND command.result_kind = 'applied'
           AND durable.issuer_kind = 'core';
        IF matched <> 1 THEN
            RAISE EXCEPTION 'lifecycle closure denial lacks core authority'
                USING ERRCODE = '23514',
                      CONSTRAINT =
                          'tool_approval_lifecycle_closure_requires_core';
        END IF;
        RETURN NULL;
    END IF;
    IF NEW.decision_source <> 'delegate' THEN
        RETURN NULL;
    END IF;
    SELECT count(*) INTO matched
      FROM tool_request AS request
      JOIN tool_approval_judge_model_call AS judge
        ON judge.request_id = request.request_id
     WHERE request.request_id = NEW.request_id
       AND request.approval_posture = 'delegated'
       AND judge.model_call_id = NEW.delegate_model_call_id
       AND judge.direct_model_selection_id = NEW.delegate_model_selection_id
       AND judge.state_kind = 'terminal'
       AND judge.terminal_disposition_kind = 'completed'
       AND judge.recommendation_kind = NEW.decision_kind
       AND judge.rationale = NEW.rationale
       AND NOT EXISTS (
            SELECT 1 FROM tool_request AS earlier
            LEFT JOIN tool_approval_decision AS earlier_decision
              ON earlier_decision.request_id = earlier.request_id
           WHERE earlier.producing_model_call_id = request.producing_model_call_id
             AND earlier.request_ordinal < request.request_ordinal
             AND earlier_decision.request_id IS NULL
       );
    IF matched <> 1 THEN
        RAISE EXCEPTION 'delegate decision lacks matching delegated authority'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'tool_approval_delegate_requires_checked_judge';
    END IF;
    RETURN NULL;
END;
$$;


--
-- Name: require_tool_attempt_authorized(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_tool_attempt_authorized() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    PERFORM assert_tool_attempt_authorized(
        CASE WHEN TG_OP = 'DELETE' THEN OLD.attempt_id ELSE NEW.attempt_id END
    );
    RETURN NULL;
END;
$$;


--
-- Name: require_tool_continuation_context_headroom_terminal(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_tool_continuation_context_headroom_terminal() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM turn_lifecycle AS lifecycle
          JOIN turn_attempt AS attempt
            ON attempt.turn_attempt_id = NEW.terminal_attempt_id
           AND attempt.turn_id = NEW.turn_id
           AND attempt.session_id = NEW.session_id
         WHERE lifecycle.turn_id = NEW.turn_id
           AND lifecycle.session_id = NEW.session_id
           AND lifecycle.state_kind = 'terminal'
           AND lifecycle.terminal_disposition_kind = 'failed'
           AND lifecycle.terminal_attempt_id = NEW.terminal_attempt_id
           AND lifecycle.terminal_model_call_id IS NULL
           AND attempt.state_kind = 'ended'
           AND attempt.end_variant = 'without_stop'
           AND attempt.end_disposition = 'known_failure'
    ) THEN
        RAISE EXCEPTION 'context-headroom marker requires its exact failed turn'
            USING
                ERRCODE = '23514',
                CONSTRAINT = 'tool_continuation_context_headroom_exact_terminal';
    END IF;
    RETURN NULL;
END;
$$;


--
-- Name: require_tool_decision_command_final_state(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_tool_decision_command_final_state() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF TG_TABLE_NAME = 'decide_tool_request_command' THEN
        PERFORM assert_tool_decision_command_final_state(NEW.command_id);
    ELSE
        PERFORM assert_tool_decision_command_final_state(NEW.user_command_id);
    END IF;
    RETURN NULL;
END;
$$;


--
-- Name: require_tool_loop_final_state(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_tool_loop_final_state() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    checked_turn uuid;
    checked_call uuid;
BEGIN
    IF TG_TABLE_NAME = 'tool_round' THEN
        checked_turn := NEW.turn_id;
        checked_call := NEW.producing_model_call_id;
    ELSIF TG_TABLE_NAME = 'tool_request' THEN
        checked_turn := NEW.turn_id;
        checked_call := NEW.producing_model_call_id;
    ELSIF TG_TABLE_NAME = 'tool_attempt' THEN
        checked_turn := CASE WHEN TG_OP = 'DELETE' THEN OLD.turn_id ELSE NEW.turn_id END;
    ELSIF TG_TABLE_NAME = 'tool_approval_decision' THEN
        SELECT turn_id, producing_model_call_id
          INTO checked_turn, checked_call
          FROM tool_request
         WHERE request_id = NEW.request_id;
    END IF;

    IF checked_call IS NOT NULL THEN
        PERFORM assert_tool_round_final_state(checked_call);
    END IF;
    IF checked_turn IS NOT NULL THEN
        PERFORM assert_turn_lifecycle_final_state(checked_turn);
    END IF;
    RETURN NULL;
END;
$$;


--
-- Name: require_user_override_authority(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_user_override_authority() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    matched bigint;
BEGIN
    SELECT count(*) INTO matched
      FROM tool_approval_decision AS denial
      JOIN semantic_transcript_entry AS denied_result
        ON denied_result.payload_kind = 'tool_denied'
       AND denied_result.tool_result_request_id = NEW.denied_request_id
     WHERE denial.request_id = NEW.denied_request_id
       AND denial.decision_kind = 'deny'
       AND denial.decision_source = 'delegate'
       AND denial.delegate_model_call_id = NEW.judge_model_call_id;
    IF matched <> 1 THEN
        RAISE EXCEPTION 'user override lacks a terminal delegate denial'
            USING ERRCODE = '23514',
                  CONSTRAINT =
                      'tool_approval_user_override_requires_terminal_denial';
    END IF;
    SELECT count(*) INTO matched
      FROM override_denied_tool_request_command AS command
     WHERE command.command_id = NEW.command_id
       AND command.result_kind = 'applied'
       AND command.request_id = NEW.denied_request_id
       AND command.session_id = NEW.session_id;
    IF matched <> 1 THEN
        RAISE EXCEPTION 'user override lacks its applied override command'
            USING ERRCODE = '23514',
                  CONSTRAINT =
                      'tool_approval_user_override_requires_applied_command';
    END IF;
    RETURN NULL;
END;
$$;


--
-- Name: stamp_eval_run_recording_transaction(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION stamp_eval_run_recording_transaction() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    NEW.recording_transaction_id := pg_catalog.pg_current_xact_id();
    RETURN NEW;
END;
$$;


--
-- Name: valid_tool_json(text); Type: FUNCTION; Schema: public
--

CREATE FUNCTION valid_tool_json(value_text text) RETURNS boolean
    LANGUAGE plpgsql IMMUTABLE STRICT
    AS $$
BEGIN
    PERFORM value_text::json;
    RETURN true;
EXCEPTION
    WHEN invalid_text_representation THEN
        RETURN false;
END;
$$;


--
-- Tables.
--

--
-- Name: approval_judge_eval_call; Type: TABLE; Schema: public
--

CREATE TABLE approval_judge_eval_call (
    eval_run_id uuid NOT NULL,
    case_name text NOT NULL,
    repeat_ordinal numeric(10,0) NOT NULL,
    recommendation_kind text NOT NULL,
    rationale text NOT NULL,
    input_tokens numeric,
    output_tokens numeric,
    cache_creation_input_tokens numeric,
    cache_read_input_tokens numeric,
    CONSTRAINT approval_judge_eval_call_case_name_nonempty CHECK ((char_length(case_name) > 0)),
    CONSTRAINT approval_judge_eval_call_rationale_bounded CHECK (((octet_length(rationale) >= 1) AND (octet_length(rationale) <= 4096))),
    CONSTRAINT approval_judge_eval_call_recommendation_closed CHECK ((recommendation_kind = ANY (ARRAY['approve'::text, 'deny'::text, 'escalate_to_human'::text]))),
    CONSTRAINT approval_judge_eval_call_repeat_ordinal_u32 CHECK (((repeat_ordinal >= (1)::numeric) AND (repeat_ordinal <= ('4294967295'::bigint)::numeric))),
    CONSTRAINT approval_judge_eval_call_usage_u64_range CHECK ((((input_tokens IS NULL) OR ((input_tokens = trunc(input_tokens)) AND ((input_tokens >= (0)::numeric) AND (input_tokens <= '18446744073709551615'::numeric)))) AND ((output_tokens IS NULL) OR ((output_tokens = trunc(output_tokens)) AND ((output_tokens >= (0)::numeric) AND (output_tokens <= '18446744073709551615'::numeric)))) AND ((cache_creation_input_tokens IS NULL) OR ((cache_creation_input_tokens = trunc(cache_creation_input_tokens)) AND ((cache_creation_input_tokens >= (0)::numeric) AND (cache_creation_input_tokens <= '18446744073709551615'::numeric)))) AND ((cache_read_input_tokens IS NULL) OR ((cache_read_input_tokens = trunc(cache_read_input_tokens)) AND ((cache_read_input_tokens >= (0)::numeric) AND (cache_read_input_tokens <= '18446744073709551615'::numeric))))))
);


--
-- Name: approval_judge_eval_run; Type: TABLE; Schema: public
--

CREATE TABLE approval_judge_eval_run (
    eval_run_id uuid NOT NULL,
    direct_model_selection_id uuid NOT NULL,
    resolved_provider_model_identity_id uuid CONSTRAINT approval_judge_eval_run_resolved_provider_model_identi_not_null NOT NULL,
    provider_model text NOT NULL,
    credential_reference text NOT NULL,
    recording_transaction_id xid8 DEFAULT pg_current_xact_id() NOT NULL,
    usage_input_includes_cache_tokens boolean CONSTRAINT approval_judge_eval_run_usage_input_includes_cache_tok_not_null NOT NULL,
    corpus_digest text NOT NULL,
    contract_digest text NOT NULL,
    rendered_digest text NOT NULL,
    repeats numeric(10,0) NOT NULL,
    scorecard jsonb NOT NULL,
    CONSTRAINT approval_judge_eval_run_credential_nonempty CHECK ((char_length(credential_reference) > 0)),
    CONSTRAINT approval_judge_eval_run_digests_nonempty CHECK (((char_length(corpus_digest) > 0) AND (char_length(contract_digest) > 0) AND (char_length(rendered_digest) > 0))),
    CONSTRAINT approval_judge_eval_run_provider_model_nonempty CHECK ((char_length(provider_model) > 0)),
    CONSTRAINT approval_judge_eval_run_repeats_u32 CHECK (((repeats >= (1)::numeric) AND (repeats <= ('4294967295'::bigint)::numeric))),
    CONSTRAINT approval_judge_eval_run_scorecard_shape CHECK ((jsonb_typeof(scorecard) = 'object'::text))
);


--
-- Name: tool_approval_judge_model_call; Type: TABLE; Schema: public
--

CREATE TABLE tool_approval_judge_model_call (
    model_call_id uuid NOT NULL,
    request_id uuid NOT NULL,
    session_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    direct_model_selection_id uuid CONSTRAINT tool_approval_judge_model_ca_direct_model_selection_id_not_null NOT NULL,
    resolved_provider_model_identity_id uuid CONSTRAINT tool_approval_judge_model_c_resolved_provider_model_id_not_null NOT NULL,
    credential_reference text NOT NULL,
    usage_input_includes_cache_tokens boolean DEFAULT false CONSTRAINT tool_approval_judge_model_c_usage_input_includes_cache_not_null NOT NULL,
    usage_provenance_kind text DEFAULT 'reported'::text NOT NULL,
    state_kind text NOT NULL,
    terminal_disposition_kind text,
    recommendation_kind text,
    rationale text,
    input_tokens numeric,
    output_tokens numeric,
    cache_read_input_tokens numeric,
    cache_creation_input_tokens numeric,
    CONSTRAINT tool_approval_judge_call_cancelled_usage_is_unreported CHECK (((terminal_disposition_kind IS DISTINCT FROM 'cancelled'::text) OR ((input_tokens IS NULL) AND (output_tokens IS NULL) AND (cache_read_input_tokens IS NULL) AND (cache_creation_input_tokens IS NULL)))),
    CONSTRAINT tool_approval_judge_call_credential_nonempty CHECK ((char_length(credential_reference) > 0)),
    CONSTRAINT tool_approval_judge_call_disposition_closed CHECK (((terminal_disposition_kind IS NULL) OR (terminal_disposition_kind = ANY (ARRAY['completed'::text, 'known_failed'::text, 'refused'::text, 'cancelled'::text, 'ambiguous'::text])))),
    CONSTRAINT tool_approval_judge_call_recommendation_closed CHECK (((recommendation_kind IS NULL) OR (recommendation_kind = ANY (ARRAY['approve'::text, 'deny'::text, 'escalate_to_human'::text])))),
    CONSTRAINT tool_approval_judge_call_state_closed CHECK ((state_kind = ANY (ARRAY['prepared'::text, 'in_flight'::text, 'terminal'::text]))),
    CONSTRAINT tool_approval_judge_call_state_shape CHECK ((((state_kind <> 'terminal'::text) AND (terminal_disposition_kind IS NULL) AND (recommendation_kind IS NULL) AND (rationale IS NULL)) OR ((state_kind = 'terminal'::text) AND (terminal_disposition_kind = 'completed'::text) AND (recommendation_kind IS NOT NULL) AND (rationale IS NOT NULL) AND ((octet_length(rationale) >= 1) AND (octet_length(rationale) <= 4096))) OR ((state_kind = 'terminal'::text) AND (terminal_disposition_kind IS NOT NULL) AND (terminal_disposition_kind <> 'completed'::text) AND (recommendation_kind IS NULL) AND (rationale IS NULL)))),
    CONSTRAINT tool_approval_judge_call_usage_provenance_closed CHECK ((usage_provenance_kind = ANY (ARRAY['reported'::text, 'estimated'::text]))),
    CONSTRAINT tool_approval_judge_call_usage_u64_range CHECK ((((input_tokens IS NULL) OR ((input_tokens = trunc(input_tokens)) AND ((input_tokens >= (0)::numeric) AND (input_tokens <= '18446744073709551615'::numeric)))) AND ((output_tokens IS NULL) OR ((output_tokens = trunc(output_tokens)) AND ((output_tokens >= (0)::numeric) AND (output_tokens <= '18446744073709551615'::numeric)))) AND ((cache_read_input_tokens IS NULL) OR ((cache_read_input_tokens = trunc(cache_read_input_tokens)) AND ((cache_read_input_tokens >= (0)::numeric) AND (cache_read_input_tokens <= '18446744073709551615'::numeric)))) AND ((cache_creation_input_tokens IS NULL) OR ((cache_creation_input_tokens = trunc(cache_creation_input_tokens)) AND ((cache_creation_input_tokens >= (0)::numeric) AND (cache_creation_input_tokens <= '18446744073709551615'::numeric))))))
);


--
-- Name: decide_tool_request_command; Type: TABLE; Schema: public
--

CREATE TABLE decide_tool_request_command (
    command_id uuid NOT NULL,
    command_kind text NOT NULL,
    storage_version smallint NOT NULL,
    request_id uuid NOT NULL,
    decision_kind text NOT NULL,
    denial_reason text,
    result_kind text NOT NULL,
    rejection_kind text,
    result_earliest_undecided_request_id uuid,
    CONSTRAINT decide_tool_request_command_decision_closed CHECK ((decision_kind = ANY (ARRAY['approve'::text, 'deny'::text]))),
    CONSTRAINT decide_tool_request_command_decision_shape CHECK ((((decision_kind = 'approve'::text) AND (denial_reason IS NULL)) OR ((decision_kind = 'deny'::text) AND ((denial_reason IS NULL) OR ((octet_length(denial_reason) BETWEEN 1 AND 1024) AND (denial_reason !~ '[[:cntrl:]]'::text) AND (denial_reason !~ '^[[:space:]]'::text) AND (denial_reason !~ '[[:space:]]$'::text)))))),
    CONSTRAINT decide_tool_request_command_kind_closed CHECK ((command_kind = 'decide_tool_request'::text)),
    CONSTRAINT decide_tool_request_command_rejection_closed CHECK (((rejection_kind IS NULL) OR (rejection_kind = ANY (ARRAY['request_not_found'::text, 'already_resolved'::text, 'not_earliest_undecided'::text])))),
    CONSTRAINT decide_tool_request_command_result_closed CHECK ((result_kind = ANY (ARRAY['applied'::text, 'rejected'::text]))),
    CONSTRAINT decide_tool_request_command_result_shape CHECK ((((result_kind = 'applied'::text) AND (rejection_kind IS NULL) AND (result_earliest_undecided_request_id IS NULL)) OR ((result_kind = 'rejected'::text) AND (rejection_kind = ANY (ARRAY['request_not_found'::text, 'already_resolved'::text])) AND (result_earliest_undecided_request_id IS NULL)) OR ((result_kind = 'rejected'::text) AND (rejection_kind = 'not_earliest_undecided'::text) AND (result_earliest_undecided_request_id IS NOT NULL) AND (result_earliest_undecided_request_id <> request_id)))),
    CONSTRAINT decide_tool_request_command_storage_version_supported CHECK ((storage_version = 1))
);


--
-- Name: evaluation_corpus; Type: TABLE; Schema: public
--

CREATE TABLE evaluation_corpus (
    corpus_name text NOT NULL COLLATE pg_catalog."C",
    corpus_version text NOT NULL COLLATE pg_catalog."C",
    format_version integer NOT NULL,
    corpus_digest bytea NOT NULL,
    replay_digest bytea NOT NULL,
    case_count bigint NOT NULL,
    source_kind text NOT NULL COLLATE pg_catalog."C",
    source_repository text,
    source_path text,
    source_sha256 bytea,
    source_blob_store text COLLATE pg_catalog."C",
    source_blob_digest bytea,
    source_blob_byte_length numeric(20,0),
    registered_at timestamp with time zone DEFAULT transaction_timestamp() NOT NULL,
    CONSTRAINT evaluation_corpus_case_count_positive CHECK ((case_count > 0)),
    CONSTRAINT evaluation_corpus_digest_sha256 CHECK ((octet_length(corpus_digest) = 32)),
    CONSTRAINT evaluation_corpus_format_version_supported CHECK ((format_version = 1)),
    CONSTRAINT evaluation_corpus_name_bounded CHECK (((octet_length(corpus_name) BETWEEN 1 AND 128) AND evaluation_corpus_text_is_nonblank_control_free(corpus_name))),
    CONSTRAINT evaluation_corpus_replay_digest_sha256 CHECK ((octet_length(replay_digest) = 32)),
    CONSTRAINT evaluation_corpus_source_kind_closed CHECK ((source_kind = ANY (ARRAY['repository'::text, 'database_native'::text, 'blob_reference'::text]))),
    CONSTRAINT evaluation_corpus_source_shape CHECK ((((source_kind = 'repository'::text) AND (source_repository IS NOT NULL) AND (octet_length(source_repository) BETWEEN 1 AND 2048) AND evaluation_corpus_text_is_nonblank_control_free(source_repository) AND (source_path IS NOT NULL) AND (octet_length(source_path) BETWEEN 1 AND 1024) AND evaluation_corpus_text_is_nonblank_control_free(source_path) AND (source_path !~ '[<>:"|?*]'::text) AND (strpos(source_path, chr(92)) = 0) AND (source_path !~ '^/'::text) AND (source_path !~ '/$'::text) AND (source_path !~ '//'::text) AND evaluation_corpus_path_components_bounded(source_path) AND (source_path !~ '(^|/)\.{1,2}(/|$)'::text) AND (source_path !~ '(^|/)[^/]*[. ](/|$)'::text) AND (source_path !~* '(^|/)(CON|PRN|AUX|NUL|CONIN[$]|CONOUT[$]|COM[1-9¹²³]|LPT[1-9¹²³])(\.|/|$)'::text) AND (source_sha256 IS NOT NULL) AND (octet_length(source_sha256) = 32) AND (source_blob_store IS NULL) AND (source_blob_digest IS NULL) AND (source_blob_byte_length IS NULL)) OR ((source_kind = 'database_native'::text) AND (source_repository IS NULL) AND (source_path IS NULL) AND (source_sha256 IS NULL) AND (source_blob_store IS NULL) AND (source_blob_digest IS NULL) AND (source_blob_byte_length IS NULL)) OR ((source_kind = 'blob_reference'::text) AND (source_repository IS NULL) AND (source_path IS NULL) AND (source_sha256 IS NULL) AND ((source_blob_store IS NULL) OR ((octet_length(source_blob_store) BETWEEN 1 AND 64) AND (source_blob_store ~ '^[a-z][a-z0-9_-]*$'::text))) AND (source_blob_digest IS NOT NULL) AND (octet_length(source_blob_digest) = 32) AND (source_blob_byte_length IS NOT NULL) AND (source_blob_byte_length >= (1)::numeric) AND (source_blob_byte_length <= '18446744073709551615'::numeric)))),
    CONSTRAINT evaluation_corpus_version_bounded CHECK (((octet_length(corpus_version) BETWEEN 1 AND 128) AND evaluation_corpus_text_is_nonblank_control_free(corpus_version)))
);


--
-- Name: evaluation_corpus_case; Type: TABLE; Schema: public
--

CREATE TABLE evaluation_corpus_case (
    corpus_name text NOT NULL COLLATE pg_catalog."C",
    corpus_version text NOT NULL COLLATE pg_catalog."C",
    case_id text NOT NULL COLLATE pg_catalog."C",
    replay_position bigint NOT NULL,
    case_json jsonb NOT NULL,
    CONSTRAINT evaluation_corpus_case_identity_bounded CHECK (((octet_length(case_id) BETWEEN 1 AND 128) AND evaluation_corpus_text_is_nonblank_control_free(case_id))),
    CONSTRAINT evaluation_corpus_case_json_object CHECK ((jsonb_typeof(case_json) = 'object'::text)),
    CONSTRAINT evaluation_corpus_case_position_nonnegative CHECK ((replay_position >= 0))
);


--
-- Name: override_denied_tool_request_command; Type: TABLE; Schema: public
--

CREATE TABLE override_denied_tool_request_command (
    command_id uuid NOT NULL,
    command_kind text NOT NULL,
    storage_version smallint NOT NULL,
    session_id uuid NOT NULL,
    request_id uuid NOT NULL,
    result_kind text NOT NULL,
    rejection_kind text,
    CONSTRAINT override_denied_tool_request_command_kind_closed CHECK ((command_kind = 'override_denied_tool_request'::text)),
    CONSTRAINT override_denied_tool_request_command_rejection_closed CHECK (((rejection_kind IS NULL) OR (rejection_kind = ANY (ARRAY['request_not_found'::text, 'request_not_in_session'::text, 'not_delegate_denied'::text, 'not_terminally_denied'::text, 'already_overridden'::text])))),
    CONSTRAINT override_denied_tool_request_command_result_closed CHECK ((result_kind = ANY (ARRAY['applied'::text, 'rejected'::text]))),
    CONSTRAINT override_denied_tool_request_command_result_shape CHECK ((((result_kind = 'applied'::text) AND (rejection_kind IS NULL)) OR ((result_kind = 'rejected'::text) AND (rejection_kind IS NOT NULL)))),
    CONSTRAINT override_denied_tool_request_command_version_supported CHECK ((storage_version = 1))
);


--
-- Name: tool_attempt; Type: TABLE; Schema: public
--

CREATE TABLE tool_attempt (
    attempt_id uuid NOT NULL,
    request_id uuid NOT NULL,
    session_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    issuing_turn_attempt_id uuid NOT NULL,
    effect_class text NOT NULL,
    dispatch_generation numeric(20,0) NOT NULL,
    state_kind text NOT NULL,
    terminal_disposition_kind text,
    result_content_kind text,
    result_text text,
    error_kind text,
    error_detail text,
    wait_spawning_request_id uuid,
    wait_child_session_id uuid,
    CONSTRAINT tool_attempt_child_wait_shape CHECK ((((terminal_disposition_kind = 'awaiting_child'::text) AND (wait_spawning_request_id IS NOT NULL) AND (wait_child_session_id IS NOT NULL)) OR ((terminal_disposition_kind IS DISTINCT FROM 'awaiting_child'::text) AND (wait_spawning_request_id IS NULL) AND (wait_child_session_id IS NULL)))),
    CONSTRAINT tool_attempt_disposition_closed CHECK (((terminal_disposition_kind IS NULL) OR (terminal_disposition_kind = ANY (ARRAY['completed'::text, 'known_failed'::text, 'awaiting_child'::text, 'ambiguous'::text])))),
    CONSTRAINT tool_attempt_effect_class_closed CHECK ((effect_class = ANY (ARRAY['effect_free'::text, 'external_effect'::text]))),
    CONSTRAINT tool_attempt_error_detail_bounded CHECK (((error_detail IS NULL) OR ((octet_length(error_detail) BETWEEN 1 AND 4096) AND (error_detail !~ '[[:cntrl:]]'::text) AND (error_detail !~ '^[[:space:]]'::text) AND (error_detail !~ '[[:space:]]$'::text)))),
    CONSTRAINT tool_attempt_error_kind_closed CHECK (((error_kind IS NULL) OR (error_kind = ANY (ARRAY['unknown_tool'::text, 'invalid_arguments'::text, 'preauthorization_rejected'::text, 'execution_failed'::text, 'result_too_large'::text, 'crash_lost'::text])))),
    CONSTRAINT tool_attempt_generation_v1 CHECK ((dispatch_generation = (1)::numeric)),
    CONSTRAINT tool_attempt_result_kind_closed CHECK (((result_content_kind IS NULL) OR (result_content_kind = 'text'::text))),
    CONSTRAINT tool_attempt_state_closed CHECK ((state_kind = ANY (ARRAY['prepared'::text, 'in_flight'::text, 'terminal'::text]))),
    CONSTRAINT tool_attempt_state_payload_shape CHECK ((((state_kind = ANY (ARRAY['prepared'::text, 'in_flight'::text])) AND (terminal_disposition_kind IS NULL) AND (result_content_kind IS NULL) AND (result_text IS NULL) AND (error_kind IS NULL) AND (error_detail IS NULL)) OR ((state_kind = 'terminal'::text) AND (terminal_disposition_kind = 'completed'::text) AND (result_content_kind = 'text'::text) AND (result_text IS NOT NULL) AND (octet_length(result_text) <= 1048576) AND (error_kind IS NULL) AND (error_detail IS NULL)) OR ((state_kind = 'terminal'::text) AND (terminal_disposition_kind = 'known_failed'::text) AND (result_content_kind IS NULL) AND (result_text IS NULL) AND (error_kind IS NOT NULL)) OR ((state_kind = 'terminal'::text) AND (terminal_disposition_kind = 'ambiguous'::text) AND (effect_class = 'external_effect'::text) AND (result_content_kind IS NULL) AND (result_text IS NULL) AND (error_kind IS NULL) AND (error_detail IS NULL)) OR ((state_kind = 'terminal'::text) AND (terminal_disposition_kind = 'awaiting_child'::text) AND (effect_class = 'effect_free'::text) AND (result_content_kind IS NULL) AND (result_text IS NULL) AND (error_kind IS NULL) AND (error_detail IS NULL))))
);


--
-- Name: tool_approval_decided_outbox_event; Type: TABLE; Schema: public
--

CREATE TABLE tool_approval_decided_outbox_event (
    event_sequence numeric(20,0) NOT NULL,
    event_kind text NOT NULL,
    storage_version smallint NOT NULL,
    session_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    request_id uuid NOT NULL,
    recording_transaction_id xid8 DEFAULT pg_current_xact_id() CONSTRAINT tool_approval_decided_outbox__recording_transaction_id_not_null NOT NULL,
    CONSTRAINT tool_approval_decided_outbox_kind_closed CHECK ((event_kind = 'tool_approval_decided'::text)),
    CONSTRAINT tool_approval_decided_outbox_version_supported CHECK ((storage_version = 1))
);


--
-- Name: tool_approval_decision; Type: TABLE; Schema: public
--

CREATE TABLE tool_approval_decision (
    request_id uuid NOT NULL,
    decision_kind text NOT NULL,
    decision_source text NOT NULL,
    denial_reason text,
    user_command_id uuid,
    delegate_model_selection_id uuid,
    delegate_model_call_id uuid,
    rationale text,
    override_denied_request_id uuid,
    CONSTRAINT tool_approval_decision_kind_closed CHECK ((decision_kind = ANY (ARRAY['approve'::text, 'deny'::text]))),
    CONSTRAINT tool_approval_decision_shape CHECK ((((decision_kind = 'approve'::text) AND (denial_reason IS NULL)) OR ((decision_kind = 'deny'::text) AND ((denial_reason IS NULL) OR ((octet_length(denial_reason) BETWEEN 1 AND 1024) AND (denial_reason !~ '[\x01-\x1f\x7f]'::text) AND (denial_reason !~ '[\u0080-\u009f]'::text) AND (denial_reason !~ '^[ \t\n\x0b\x0c\r]'::text) AND (denial_reason !~ '[ \t\n\x0b\x0c\r]$'::text)))))),
    CONSTRAINT tool_approval_decision_source_closed CHECK ((decision_source = ANY (ARRAY['user_command'::text, 'policy_auto'::text, 'session_blanket'::text, 'delegate'::text, 'user_override'::text, 'runtime_safety'::text, 'lifecycle_closure'::text]))),
    CONSTRAINT tool_approval_decision_source_shape CHECK ((((decision_source = 'user_command'::text) AND (user_command_id IS NOT NULL) AND (delegate_model_selection_id IS NULL) AND (delegate_model_call_id IS NULL) AND (rationale IS NULL) AND (override_denied_request_id IS NULL)) OR ((decision_source = ANY (ARRAY['policy_auto'::text, 'session_blanket'::text])) AND (decision_kind = 'approve'::text) AND (user_command_id IS NULL) AND (delegate_model_selection_id IS NULL) AND (delegate_model_call_id IS NULL) AND (rationale IS NULL) AND (override_denied_request_id IS NULL)) OR ((decision_source = 'delegate'::text) AND (user_command_id IS NULL) AND (delegate_model_selection_id IS NOT NULL) AND (delegate_model_call_id IS NOT NULL) AND (rationale IS NOT NULL) AND ((octet_length(rationale) >= 1) AND (octet_length(rationale) <= 4096)) AND (override_denied_request_id IS NULL)) OR ((decision_source = 'user_override'::text) AND (decision_kind = 'approve'::text) AND (user_command_id IS NULL) AND (delegate_model_selection_id IS NULL) AND (delegate_model_call_id IS NULL) AND (rationale IS NULL) AND (override_denied_request_id IS NOT NULL)) OR ((decision_source = 'runtime_safety'::text) AND (decision_kind = 'deny'::text) AND (denial_reason = 'Tool arguments were suppressed by the credential boundary'::text) AND (user_command_id IS NULL) AND (delegate_model_selection_id IS NULL) AND (delegate_model_call_id IS NULL) AND (rationale IS NULL) AND (override_denied_request_id IS NULL)) OR ((decision_source = 'lifecycle_closure'::text) AND (decision_kind = 'deny'::text) AND (denial_reason IS NULL) AND (user_command_id IS NOT NULL) AND (delegate_model_selection_id IS NULL) AND (delegate_model_call_id IS NULL) AND (rationale IS NULL) AND (override_denied_request_id IS NULL))))
);


--
-- Name: tool_approval_user_override; Type: TABLE; Schema: public
--

CREATE TABLE tool_approval_user_override (
    denied_request_id uuid NOT NULL,
    session_id uuid NOT NULL,
    command_id uuid NOT NULL,
    judge_model_call_id uuid NOT NULL
);


--
-- Name: tool_batch_transition_outbox_event; Type: TABLE; Schema: public
--

CREATE TABLE tool_batch_transition_outbox_event (
    event_sequence numeric(20,0) NOT NULL,
    event_kind text NOT NULL,
    storage_version smallint NOT NULL,
    session_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    producing_model_call_id uuid CONSTRAINT tool_batch_transition_outbox_e_producing_model_call_id_not_null NOT NULL,
    transition_kind text NOT NULL,
    frontier_id uuid,
    tool_attempt_id uuid,
    CONSTRAINT tool_batch_transition_outbox_kind_closed CHECK ((event_kind = 'tool_batch_transition'::text)),
    CONSTRAINT tool_batch_transition_outbox_shape CHECK ((((transition_kind = ANY (ARRAY['proposed'::text, 'results_projected'::text])) AND (frontier_id IS NOT NULL) AND (tool_attempt_id IS NULL)) OR ((transition_kind = 'recovery_required'::text) AND (frontier_id IS NULL) AND (tool_attempt_id IS NOT NULL)))),
    CONSTRAINT tool_batch_transition_outbox_state_closed CHECK ((transition_kind = ANY (ARRAY['proposed'::text, 'results_projected'::text, 'recovery_required'::text]))),
    CONSTRAINT tool_batch_transition_outbox_version_supported CHECK ((storage_version = 1))
);


--
-- Name: tool_continuation_context_headroom; Type: TABLE; Schema: public
--

CREATE TABLE tool_continuation_context_headroom (
    terminal_attempt_id uuid NOT NULL,
    producing_model_call_id uuid CONSTRAINT tool_continuation_context_head_producing_model_call_id_not_null NOT NULL,
    session_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    usage_input_includes_cache_tokens boolean CONSTRAINT tool_continuation_context_h_usage_input_includes_cache_not_null NOT NULL,
    usage_input_tokens numeric(20,0) NOT NULL,
    usage_output_tokens numeric(20,0),
    usage_cache_creation_input_tokens numeric(20,0),
    usage_cache_read_input_tokens numeric(20,0),
    max_output_tokens numeric(20,0) NOT NULL,
    context_window_tokens numeric(20,0) CONSTRAINT tool_continuation_context_headro_context_window_tokens_not_null NOT NULL,
    recorded_at timestamp with time zone DEFAULT transaction_timestamp() NOT NULL,
    projected_result_content_bytes numeric(20,0) CONSTRAINT tool_continuation_context_h_projected_result_content_b_not_null NOT NULL,
    CONSTRAINT tool_continuation_context_headroom_limits_positive CHECK (((max_output_tokens > (0)::numeric) AND (context_window_tokens > (0)::numeric) AND (max_output_tokens <= context_window_tokens))),
    CONSTRAINT tool_continuation_context_headroom_requires_compaction CHECK ((((((usage_input_tokens +
CASE
    WHEN usage_input_includes_cache_tokens THEN (0)::numeric
    ELSE (COALESCE(usage_cache_creation_input_tokens, (0)::numeric) + COALESCE(usage_cache_read_input_tokens, (0)::numeric))
END) + COALESCE(usage_output_tokens, (0)::numeric)) + projected_result_content_bytes) + max_output_tokens) > context_window_tokens)),
    CONSTRAINT tool_continuation_context_headroom_result_bytes_nonnegative CHECK ((projected_result_content_bytes >= (0)::numeric)),
    CONSTRAINT tool_continuation_context_headroom_usage_nonnegative CHECK (((usage_input_tokens >= (0)::numeric) AND ((usage_output_tokens IS NULL) OR (usage_output_tokens >= (0)::numeric)) AND ((usage_cache_creation_input_tokens IS NULL) OR (usage_cache_creation_input_tokens >= (0)::numeric)) AND ((usage_cache_read_input_tokens IS NULL) OR (usage_cache_read_input_tokens >= (0)::numeric))))
);


--
-- Name: tool_request; Type: TABLE; Schema: public
--

CREATE TABLE tool_request (
    request_id uuid NOT NULL,
    session_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    producing_model_call_id uuid NOT NULL,
    request_ordinal numeric(10,0) NOT NULL,
    tool_name text NOT NULL,
    arguments_kind text NOT NULL,
    arguments_text text NOT NULL,
    approval_posture text DEFAULT 'human'::text NOT NULL,
    CONSTRAINT tool_request_approval_posture_closed CHECK ((approval_posture = ANY (ARRAY['auto'::text, 'delegated'::text, 'human'::text]))),
    CONSTRAINT tool_request_arguments_bounded CHECK ((octet_length(arguments_text) <= 1048576)),
    CONSTRAINT tool_request_arguments_kind_closed CHECK ((arguments_kind = ANY (ARRAY['json'::text, 'undecodable'::text]))),
    CONSTRAINT tool_request_arguments_representation CHECK ((((arguments_kind = 'json'::text) AND (canonical_tool_json(arguments_text) IS NOT NULL) AND (arguments_text = canonical_tool_json(arguments_text))) OR ((arguments_kind = 'undecodable'::text) AND (NOT valid_tool_json(arguments_text))))),
    CONSTRAINT tool_request_name_shape CHECK (((octet_length(tool_name) BETWEEN 1 AND 64) AND (tool_name ~ '^[A-Za-z0-9_-]+$'::text))),
    CONSTRAINT tool_request_ordinal_u32 CHECK (((request_ordinal >= (0)::numeric) AND (request_ordinal <= ('4294967295'::bigint)::numeric)))
);


--
-- Name: tool_round; Type: TABLE; Schema: public
--

CREATE TABLE tool_round (
    producing_model_call_id uuid NOT NULL,
    session_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    boundary_kind text NOT NULL,
    boundary_frontier_id uuid NOT NULL,
    response_part_count numeric(20,0) NOT NULL,
    request_count numeric(20,0) NOT NULL,
    CONSTRAINT tool_round_boundary_kind_closed CHECK ((boundary_kind = ANY (ARRAY['continuing'::text, 'closed_by_turn_end'::text]))),
    CONSTRAINT tool_round_counts_bounded CHECK (((response_part_count BETWEEN (1)::numeric AND ('4294967295'::bigint)::numeric) AND (request_count BETWEEN (1)::numeric AND (32)::numeric) AND (request_count <= response_part_count)))
);


--
-- Constraints.
--

--
-- Name: approval_judge_eval_call approval_judge_eval_call_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY approval_judge_eval_call
    ADD CONSTRAINT approval_judge_eval_call_pkey PRIMARY KEY (eval_run_id, case_name, repeat_ordinal);


--
-- Name: approval_judge_eval_run approval_judge_eval_run_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY approval_judge_eval_run
    ADD CONSTRAINT approval_judge_eval_run_pkey PRIMARY KEY (eval_run_id);


--
-- Name: decide_tool_request_command decide_tool_request_command_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY decide_tool_request_command
    ADD CONSTRAINT decide_tool_request_command_pkey PRIMARY KEY (command_id);


--
-- Name: evaluation_corpus_case evaluation_corpus_case_pk; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY evaluation_corpus_case
    ADD CONSTRAINT evaluation_corpus_case_pk PRIMARY KEY (corpus_name, corpus_version, case_id);


--
-- Name: evaluation_corpus_case evaluation_corpus_case_position_unique; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY evaluation_corpus_case
    ADD CONSTRAINT evaluation_corpus_case_position_unique UNIQUE (corpus_name, corpus_version, replay_position);


--
-- Name: evaluation_corpus evaluation_corpus_pk; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY evaluation_corpus
    ADD CONSTRAINT evaluation_corpus_pk PRIMARY KEY (corpus_name, corpus_version);


--
-- Name: override_denied_tool_request_command override_denied_tool_request_command_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY override_denied_tool_request_command
    ADD CONSTRAINT override_denied_tool_request_command_pkey PRIMARY KEY (command_id);


--
-- Name: tool_approval_decided_outbox_event tool_approval_decided_outbox_event_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY tool_approval_decided_outbox_event
    ADD CONSTRAINT tool_approval_decided_outbox_event_pkey PRIMARY KEY (event_sequence);


--
-- Name: tool_approval_decided_outbox_event tool_approval_decided_outbox_event_request_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY tool_approval_decided_outbox_event
    ADD CONSTRAINT tool_approval_decided_outbox_event_request_id_key UNIQUE (request_id);


--
-- Name: tool_approval_decision tool_approval_decision_delegate_model_call_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY tool_approval_decision
    ADD CONSTRAINT tool_approval_decision_delegate_model_call_id_key UNIQUE (delegate_model_call_id);


--
-- Name: tool_approval_decision tool_approval_decision_override_denied_request_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY tool_approval_decision
    ADD CONSTRAINT tool_approval_decision_override_denied_request_id_key UNIQUE (override_denied_request_id);


--
-- Name: tool_approval_decision tool_approval_decision_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY tool_approval_decision
    ADD CONSTRAINT tool_approval_decision_pkey PRIMARY KEY (request_id);


--
-- Name: tool_approval_decision tool_approval_decision_user_command_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY tool_approval_decision
    ADD CONSTRAINT tool_approval_decision_user_command_id_key UNIQUE (user_command_id);


--
-- Name: tool_approval_judge_model_call tool_approval_judge_call_session_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY tool_approval_judge_model_call
    ADD CONSTRAINT tool_approval_judge_call_session_key UNIQUE (model_call_id, session_id);


--
-- Name: tool_approval_judge_model_call tool_approval_judge_model_call_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY tool_approval_judge_model_call
    ADD CONSTRAINT tool_approval_judge_model_call_pkey PRIMARY KEY (model_call_id);


--
-- Name: tool_approval_judge_model_call tool_approval_judge_model_call_request_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY tool_approval_judge_model_call
    ADD CONSTRAINT tool_approval_judge_model_call_request_id_key UNIQUE (request_id);


--
-- Name: tool_approval_user_override tool_approval_user_override_command_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY tool_approval_user_override
    ADD CONSTRAINT tool_approval_user_override_command_id_key UNIQUE (command_id);


--
-- Name: tool_approval_user_override tool_approval_user_override_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY tool_approval_user_override
    ADD CONSTRAINT tool_approval_user_override_pkey PRIMARY KEY (denied_request_id);


--
-- Name: tool_attempt tool_attempt_correlation_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY tool_attempt
    ADD CONSTRAINT tool_attempt_correlation_key UNIQUE (attempt_id, request_id, issuing_turn_attempt_id, dispatch_generation);


--
-- Name: tool_attempt tool_attempt_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY tool_attempt
    ADD CONSTRAINT tool_attempt_pkey PRIMARY KEY (attempt_id);


--
-- Name: tool_attempt tool_attempt_session_correlation_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY tool_attempt
    ADD CONSTRAINT tool_attempt_session_correlation_key UNIQUE (attempt_id, session_id);


--
-- Name: tool_attempt tool_attempt_turn_correlation_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY tool_attempt
    ADD CONSTRAINT tool_attempt_turn_correlation_key UNIQUE (attempt_id, turn_id, session_id);


--
-- Name: tool_batch_transition_outbox_event tool_batch_transition_outbox_event_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY tool_batch_transition_outbox_event
    ADD CONSTRAINT tool_batch_transition_outbox_event_pkey PRIMARY KEY (event_sequence);


--
-- Name: tool_continuation_context_headroom tool_continuation_context_headroom_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY tool_continuation_context_headroom
    ADD CONSTRAINT tool_continuation_context_headroom_pkey PRIMARY KEY (terminal_attempt_id);


--
-- Name: tool_continuation_context_headroom tool_continuation_context_headroom_producing_model_call_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY tool_continuation_context_headroom
    ADD CONSTRAINT tool_continuation_context_headroom_producing_model_call_id_key UNIQUE (producing_model_call_id);


--
-- Name: tool_request tool_request_call_ordinal_once; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY tool_request
    ADD CONSTRAINT tool_request_call_ordinal_once UNIQUE (producing_model_call_id, request_ordinal);


--
-- Name: tool_request tool_request_correlation_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY tool_request
    ADD CONSTRAINT tool_request_correlation_key UNIQUE (request_id, producing_model_call_id, session_id);


--
-- Name: tool_request tool_request_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY tool_request
    ADD CONSTRAINT tool_request_pkey PRIMARY KEY (request_id);


--
-- Name: tool_request tool_request_session_correlation_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY tool_request
    ADD CONSTRAINT tool_request_session_correlation_key UNIQUE (request_id, session_id);


--
-- Name: tool_request tool_request_turn_correlation_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY tool_request
    ADD CONSTRAINT tool_request_turn_correlation_key UNIQUE (request_id, turn_id, session_id);


--
-- Name: tool_round tool_round_call_correlation_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY tool_round
    ADD CONSTRAINT tool_round_call_correlation_key UNIQUE (producing_model_call_id, turn_id, session_id);


--
-- Name: tool_round tool_round_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY tool_round
    ADD CONSTRAINT tool_round_pkey PRIMARY KEY (producing_model_call_id);


--
-- Indexes.
--

--
-- Name: tool_approval_judge_usage_by_session_state_turn_call; Type: INDEX; Schema: public
--

CREATE INDEX tool_approval_judge_usage_by_session_state_turn_call ON tool_approval_judge_model_call USING btree (session_id, state_kind, turn_id, model_call_id);


--
-- Name: tool_approval_user_override_session_request_idx; Type: INDEX; Schema: public
--

CREATE INDEX tool_approval_user_override_session_request_idx ON tool_approval_user_override USING btree (session_id, denied_request_id);


--
-- Name: tool_attempt_live_by_session; Type: INDEX; Schema: public
--

CREATE INDEX tool_attempt_live_by_session ON tool_attempt USING btree (session_id) WHERE (state_kind <> 'terminal'::text);


--
-- Name: tool_attempt_one_live_per_turn; Type: INDEX; Schema: public
--

CREATE UNIQUE INDEX tool_attempt_one_live_per_turn ON tool_attempt USING btree (turn_id) WHERE (state_kind <> 'terminal'::text);


--
-- Name: tool_attempt_request_id_idx; Type: INDEX; Schema: public
--

CREATE INDEX tool_attempt_request_id_idx ON tool_attempt USING btree (request_id);


--
-- Name: tool_batch_transition_outbox_frontier_once; Type: INDEX; Schema: public
--

CREATE UNIQUE INDEX tool_batch_transition_outbox_frontier_once ON tool_batch_transition_outbox_event USING btree (producing_model_call_id, transition_kind) WHERE (transition_kind = ANY (ARRAY['proposed'::text, 'results_projected'::text]));


--
-- Name: tool_batch_transition_outbox_recovery_once; Type: INDEX; Schema: public
--

CREATE UNIQUE INDEX tool_batch_transition_outbox_recovery_once ON tool_batch_transition_outbox_event USING btree (tool_attempt_id) WHERE (transition_kind = 'recovery_required'::text);


--
-- Name: tool_request_session_tool_name_idx; Type: INDEX; Schema: public
--

CREATE INDEX tool_request_session_tool_name_idx ON tool_request USING btree (session_id, tool_name);


--
-- Triggers.
--

--
-- Name: approval_judge_eval_call approval_judge_eval_call_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER approval_judge_eval_call_cannot_be_truncated BEFORE TRUNCATE ON approval_judge_eval_call FOR EACH STATEMENT EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: approval_judge_eval_call approval_judge_eval_call_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER approval_judge_eval_call_is_append_only BEFORE DELETE OR UPDATE ON approval_judge_eval_call FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: approval_judge_eval_call approval_judge_eval_call_is_sealed_with_its_run; Type: TRIGGER; Schema: public
--

CREATE TRIGGER approval_judge_eval_call_is_sealed_with_its_run BEFORE INSERT ON approval_judge_eval_call FOR EACH ROW EXECUTE FUNCTION reject_eval_call_outside_run_recording();


--
-- Name: approval_judge_eval_run approval_judge_eval_run_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER approval_judge_eval_run_cannot_be_truncated BEFORE TRUNCATE ON approval_judge_eval_run FOR EACH STATEMENT EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: approval_judge_eval_run approval_judge_eval_run_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER approval_judge_eval_run_is_append_only BEFORE DELETE OR UPDATE ON approval_judge_eval_run FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: approval_judge_eval_run approval_judge_eval_run_stamps_its_recording_transaction; Type: TRIGGER; Schema: public
--

CREATE TRIGGER approval_judge_eval_run_stamps_its_recording_transaction BEFORE INSERT ON approval_judge_eval_run FOR EACH ROW EXECUTE FUNCTION stamp_eval_run_recording_transaction();


--
-- Name: tool_approval_judge_model_call approval_judge_projects_terminal_usage; Type: TRIGGER; Schema: public
--

CREATE TRIGGER approval_judge_projects_terminal_usage AFTER INSERT OR UPDATE ON tool_approval_judge_model_call FOR EACH ROW WHEN ((new.state_kind = 'terminal'::text)) EXECUTE FUNCTION project_terminal_approval_judge_usage();


--
-- Name: decide_tool_request_command decide_tool_request_command_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER decide_tool_request_command_is_append_only BEFORE DELETE OR UPDATE ON decide_tool_request_command FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: decide_tool_request_command decide_tool_request_command_requires_effect; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER decide_tool_request_command_requires_effect AFTER INSERT ON decide_tool_request_command DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_tool_decision_command_final_state();


--
-- Name: tool_approval_decision denied_tool_request_has_no_attempt; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER denied_tool_request_has_no_attempt AFTER INSERT ON tool_approval_decision DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_denied_tool_without_attempt();


--
-- Name: override_denied_tool_request_command override_denied_tool_request_command_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER override_denied_tool_request_command_is_append_only BEFORE DELETE OR UPDATE ON override_denied_tool_request_command FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: override_denied_tool_request_command override_denied_tool_request_command_requires_effect; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER override_denied_tool_request_command_requires_effect AFTER INSERT ON override_denied_tool_request_command DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_override_command_effect();


--
-- Name: queued_input_origin queued_input_origin_defaults_v1_tool_auto_approval; Type: TRIGGER; Schema: public
--

CREATE TRIGGER queued_input_origin_defaults_v1_tool_auto_approval BEFORE INSERT ON queued_input_origin FOR EACH ROW EXECUTE FUNCTION default_v1_queued_tool_auto_approval();


--
-- Name: semantic_transcript_entry semantic_entry_one_logical_tool_result; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER semantic_entry_one_logical_tool_result AFTER INSERT OR DELETE OR UPDATE ON semantic_transcript_entry DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_single_tool_result();


--
-- Name: tool_approval_decided_outbox_event tool_approval_decided_outbox_event_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER tool_approval_decided_outbox_event_cannot_be_truncated BEFORE TRUNCATE ON tool_approval_decided_outbox_event FOR EACH STATEMENT EXECUTE FUNCTION reject_outbox_table_truncate();


--
-- Name: tool_approval_decided_outbox_event tool_approval_decided_outbox_event_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER tool_approval_decided_outbox_event_is_append_only BEFORE DELETE OR UPDATE ON tool_approval_decided_outbox_event FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: tool_approval_decided_outbox_event tool_approval_decided_requires_explicit_source; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER tool_approval_decided_requires_explicit_source AFTER INSERT ON tool_approval_decided_outbox_event DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_explicit_tool_approval_decided_outbox();


--
-- Name: tool_approval_decision tool_approval_decision_authority; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER tool_approval_decision_authority AFTER INSERT OR UPDATE ON tool_approval_decision DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_tool_approval_decision_authority();


--
-- Name: tool_approval_decision tool_approval_decision_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER tool_approval_decision_is_append_only BEFORE DELETE OR UPDATE ON tool_approval_decision FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: tool_approval_decision tool_approval_explicit_requires_atomic_effect; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER tool_approval_explicit_requires_atomic_effect AFTER INSERT OR UPDATE ON tool_approval_decision DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_explicit_tool_approval_effect();


--
-- Name: tool_approval_judge_model_call tool_approval_judge_call_changes_are_guarded; Type: TRIGGER; Schema: public
--

CREATE TRIGGER tool_approval_judge_call_changes_are_guarded BEFORE INSERT OR DELETE OR UPDATE ON tool_approval_judge_model_call FOR EACH ROW EXECUTE FUNCTION reject_tool_approval_judge_call_invalid_change();


--
-- Name: tool_approval_judge_model_call tool_approval_judge_call_reserves_global_identity; Type: TRIGGER; Schema: public
--

CREATE TRIGGER tool_approval_judge_call_reserves_global_identity BEFORE INSERT ON tool_approval_judge_model_call FOR EACH ROW EXECUTE FUNCTION reserve_model_call_identity('approval_judge');


--
-- Name: tool_approval_judge_model_call tool_approval_judge_completed_requires_decision_effect; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER tool_approval_judge_completed_requires_decision_effect AFTER INSERT OR UPDATE ON tool_approval_judge_model_call DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_completed_tool_approval_judge_decision();


--
-- Name: tool_approval_decision tool_approval_requires_complete_final_state; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER tool_approval_requires_complete_final_state AFTER INSERT OR DELETE OR UPDATE ON tool_approval_decision DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_tool_loop_final_state();


--
-- Name: tool_approval_decision tool_approval_session_blanket_provenance; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER tool_approval_session_blanket_provenance AFTER INSERT OR UPDATE ON tool_approval_decision DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_session_blanket_approval_provenance();


--
-- Name: tool_approval_user_override tool_approval_user_override_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER tool_approval_user_override_is_append_only BEFORE DELETE OR UPDATE ON tool_approval_user_override FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: tool_attempt tool_attempt_changes_are_guarded; Type: TRIGGER; Schema: public
--

CREATE TRIGGER tool_attempt_changes_are_guarded BEFORE INSERT OR DELETE OR UPDATE ON tool_attempt FOR EACH ROW EXECUTE FUNCTION reject_tool_attempt_invalid_change();


--
-- Name: tool_attempt tool_attempt_rechecks_turn_runner_recovery; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER tool_attempt_rechecks_turn_runner_recovery AFTER INSERT OR DELETE OR UPDATE ON tool_attempt DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_turn_runner_recovery_complete();


--
-- Name: tool_attempt tool_attempt_requires_approval; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER tool_attempt_requires_approval AFTER INSERT OR DELETE OR UPDATE ON tool_attempt DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_tool_attempt_authorized();


--
-- Name: tool_attempt tool_attempt_requires_complete_final_state; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER tool_attempt_requires_complete_final_state AFTER INSERT OR DELETE OR UPDATE ON tool_attempt DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_tool_loop_final_state();


--
-- Name: tool_batch_transition_outbox_event tool_batch_transition_outbox_event_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER tool_batch_transition_outbox_event_cannot_be_truncated BEFORE TRUNCATE ON tool_batch_transition_outbox_event FOR EACH STATEMENT EXECUTE FUNCTION reject_outbox_table_truncate();


--
-- Name: tool_batch_transition_outbox_event tool_batch_transition_outbox_event_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER tool_batch_transition_outbox_event_is_append_only BEFORE DELETE OR UPDATE ON tool_batch_transition_outbox_event FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: tool_continuation_context_headroom tool_continuation_context_headroom_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER tool_continuation_context_headroom_is_append_only BEFORE DELETE OR UPDATE ON tool_continuation_context_headroom FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: tool_continuation_context_headroom tool_continuation_context_headroom_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER tool_continuation_context_headroom_reject_truncate BEFORE TRUNCATE ON tool_continuation_context_headroom FOR EACH STATEMENT EXECUTE FUNCTION reject_tool_continuation_context_headroom_truncate();


--
-- Name: tool_continuation_context_headroom tool_continuation_context_headroom_requires_terminal; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER tool_continuation_context_headroom_requires_terminal AFTER INSERT ON tool_continuation_context_headroom DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_tool_continuation_context_headroom_terminal();


--
-- Name: tool_request tool_request_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER tool_request_is_append_only BEFORE DELETE OR UPDATE ON tool_request FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: tool_request tool_request_requires_complete_final_state; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER tool_request_requires_complete_final_state AFTER INSERT OR DELETE OR UPDATE ON tool_request DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_tool_loop_final_state();


--
-- Name: tool_round tool_round_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER tool_round_is_append_only BEFORE DELETE OR UPDATE ON tool_round FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: tool_round tool_round_rechecks_turn_runner_recovery; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER tool_round_rechecks_turn_runner_recovery AFTER INSERT OR DELETE OR UPDATE ON tool_round DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_turn_runner_recovery_complete();


--
-- Name: tool_round tool_round_requires_complete_final_state; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER tool_round_requires_complete_final_state AFTER INSERT OR DELETE OR UPDATE ON tool_round DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_tool_loop_final_state();


--
-- Name: tool_approval_user_override user_override_requires_authority; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER user_override_requires_authority AFTER INSERT ON tool_approval_user_override DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_user_override_authority();


--
-- Name: tool_approval_decision user_tool_approval_requires_command; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER user_tool_approval_requires_command AFTER INSERT ON tool_approval_decision DEFERRABLE INITIALLY DEFERRED FOR EACH ROW WHEN ((new.user_command_id IS NOT NULL)) EXECUTE FUNCTION require_tool_decision_command_final_state();


--
-- Foreign keys.
--

--
-- Name: approval_judge_eval_call approval_judge_eval_call_run_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY approval_judge_eval_call
    ADD CONSTRAINT approval_judge_eval_call_run_fk FOREIGN KEY (eval_run_id) REFERENCES approval_judge_eval_run(eval_run_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: decide_tool_request_command decide_tool_request_command_registry_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY decide_tool_request_command
    ADD CONSTRAINT decide_tool_request_command_registry_fk FOREIGN KEY (command_id, command_kind, storage_version) REFERENCES durable_command(command_id, command_kind, storage_version) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: evaluation_corpus_case evaluation_corpus_case_corpus_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY evaluation_corpus_case
    ADD CONSTRAINT evaluation_corpus_case_corpus_fk FOREIGN KEY (corpus_name, corpus_version) REFERENCES evaluation_corpus(corpus_name, corpus_version) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: override_denied_tool_request_command override_denied_tool_request_command_registry_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY override_denied_tool_request_command
    ADD CONSTRAINT override_denied_tool_request_command_registry_fk FOREIGN KEY (command_id, command_kind, storage_version) REFERENCES durable_command(command_id, command_kind, storage_version) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: tool_approval_decided_outbox_event tool_approval_decided_outbox_decision_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY tool_approval_decided_outbox_event
    ADD CONSTRAINT tool_approval_decided_outbox_decision_fk FOREIGN KEY (request_id) REFERENCES tool_approval_decision(request_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: tool_approval_decided_outbox_event tool_approval_decided_outbox_header_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY tool_approval_decided_outbox_event
    ADD CONSTRAINT tool_approval_decided_outbox_header_fk FOREIGN KEY (event_sequence, event_kind, storage_version, session_id) REFERENCES outbox_event(event_sequence, event_kind, storage_version, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: tool_approval_decided_outbox_event tool_approval_decided_outbox_request_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY tool_approval_decided_outbox_event
    ADD CONSTRAINT tool_approval_decided_outbox_request_fk FOREIGN KEY (request_id, turn_id, session_id) REFERENCES tool_request(request_id, turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: tool_approval_decision tool_approval_decision_delegate_call_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY tool_approval_decision
    ADD CONSTRAINT tool_approval_decision_delegate_call_fk FOREIGN KEY (delegate_model_call_id) REFERENCES tool_approval_judge_model_call(model_call_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: tool_approval_decision tool_approval_decision_override_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY tool_approval_decision
    ADD CONSTRAINT tool_approval_decision_override_fk FOREIGN KEY (override_denied_request_id) REFERENCES tool_approval_user_override(denied_request_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: tool_approval_decision tool_approval_decision_request_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY tool_approval_decision
    ADD CONSTRAINT tool_approval_decision_request_fk FOREIGN KEY (request_id) REFERENCES tool_request(request_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: tool_approval_decision tool_approval_decision_user_command_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY tool_approval_decision
    ADD CONSTRAINT tool_approval_decision_user_command_fk FOREIGN KEY (user_command_id) REFERENCES decide_tool_request_command(command_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: tool_approval_judge_model_call tool_approval_judge_call_request_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY tool_approval_judge_model_call
    ADD CONSTRAINT tool_approval_judge_call_request_fk FOREIGN KEY (request_id, turn_id, session_id) REFERENCES tool_request(request_id, turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: tool_approval_judge_model_call tool_approval_judge_call_session_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY tool_approval_judge_model_call
    ADD CONSTRAINT tool_approval_judge_call_session_fk FOREIGN KEY (session_id) REFERENCES session(session_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: tool_approval_user_override tool_approval_user_override_command_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY tool_approval_user_override
    ADD CONSTRAINT tool_approval_user_override_command_fk FOREIGN KEY (command_id) REFERENCES override_denied_tool_request_command(command_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: tool_approval_user_override tool_approval_user_override_denial_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY tool_approval_user_override
    ADD CONSTRAINT tool_approval_user_override_denial_fk FOREIGN KEY (denied_request_id) REFERENCES tool_approval_decision(request_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: tool_approval_user_override tool_approval_user_override_judge_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY tool_approval_user_override
    ADD CONSTRAINT tool_approval_user_override_judge_fk FOREIGN KEY (judge_model_call_id) REFERENCES tool_approval_judge_model_call(model_call_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: tool_approval_user_override tool_approval_user_override_request_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY tool_approval_user_override
    ADD CONSTRAINT tool_approval_user_override_request_fk FOREIGN KEY (denied_request_id, session_id) REFERENCES tool_request(request_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: tool_attempt tool_attempt_issuing_turn_attempt_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY tool_attempt
    ADD CONSTRAINT tool_attempt_issuing_turn_attempt_fk FOREIGN KEY (issuing_turn_attempt_id, turn_id, session_id) REFERENCES turn_attempt(turn_attempt_id, turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: tool_attempt tool_attempt_request_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY tool_attempt
    ADD CONSTRAINT tool_attempt_request_fk FOREIGN KEY (request_id, turn_id, session_id) REFERENCES tool_request(request_id, turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: tool_batch_transition_outbox_event tool_batch_transition_outbox_attempt_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY tool_batch_transition_outbox_event
    ADD CONSTRAINT tool_batch_transition_outbox_attempt_fk FOREIGN KEY (tool_attempt_id, turn_id, session_id) REFERENCES tool_attempt(attempt_id, turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: tool_batch_transition_outbox_event tool_batch_transition_outbox_frontier_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY tool_batch_transition_outbox_event
    ADD CONSTRAINT tool_batch_transition_outbox_frontier_fk FOREIGN KEY (session_id, frontier_id) REFERENCES context_frontier(owning_session_id, context_frontier_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: tool_batch_transition_outbox_event tool_batch_transition_outbox_header_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY tool_batch_transition_outbox_event
    ADD CONSTRAINT tool_batch_transition_outbox_header_fk FOREIGN KEY (event_sequence, event_kind, storage_version, session_id) REFERENCES outbox_event(event_sequence, event_kind, storage_version, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: tool_batch_transition_outbox_event tool_batch_transition_outbox_round_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY tool_batch_transition_outbox_event
    ADD CONSTRAINT tool_batch_transition_outbox_round_fk FOREIGN KEY (producing_model_call_id, turn_id, session_id) REFERENCES tool_round(producing_model_call_id, turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: tool_continuation_context_headroom tool_continuation_context_headroom_attempt_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY tool_continuation_context_headroom
    ADD CONSTRAINT tool_continuation_context_headroom_attempt_fk FOREIGN KEY (terminal_attempt_id, turn_id, session_id) REFERENCES turn_attempt(turn_attempt_id, turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: tool_continuation_context_headroom tool_continuation_context_headroom_round_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY tool_continuation_context_headroom
    ADD CONSTRAINT tool_continuation_context_headroom_round_fk FOREIGN KEY (producing_model_call_id, turn_id, session_id) REFERENCES tool_round(producing_model_call_id, turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: tool_request tool_request_round_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY tool_request
    ADD CONSTRAINT tool_request_round_fk FOREIGN KEY (producing_model_call_id, turn_id, session_id) REFERENCES tool_round(producing_model_call_id, turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: tool_round tool_round_call_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY tool_round
    ADD CONSTRAINT tool_round_call_fk FOREIGN KEY (producing_model_call_id, turn_id, session_id) REFERENCES model_call(model_call_id, turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: tool_round tool_round_frontier_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY tool_round
    ADD CONSTRAINT tool_round_frontier_fk FOREIGN KEY (session_id, boundary_frontier_id) REFERENCES context_frontier(owning_session_id, context_frontier_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Search-path pins for this file's constraint-reachable functions.
--
-- The pin has to name the schema the migration selected rather than a
-- literal, so it is applied here through current_schema instead of inline
-- in each CREATE FUNCTION (the full rationale is in 202609010000_core.sql;
-- crates/persistence/tests/search_path_postgres.rs is the guard).
--

DO $search_path_pins$
DECLARE
    signature text;
BEGIN
    -- a pin that names no user schema, so it needs no substitution
    FOREACH signature IN ARRAY ARRAY[
        'reject_eval_call_outside_run_recording()',
        'stamp_eval_run_recording_transaction()'
    ] LOOP
        EXECUTE format('ALTER FUNCTION %s SET search_path TO pg_catalog, pg_temp',
                   signature);
    END LOOP;
    -- the canonical restore-safe pin: the migration-selected schema, then pg_catalog, then pg_temp
    FOREACH signature IN ARRAY ARRAY[
        'canonical_tool_json(text)',
        'canonical_tool_json_number(text)',
        'evaluation_corpus_path_components_bounded(text)',
        'evaluation_corpus_text_is_nonblank_control_free(text)',
        'valid_tool_json(text)'
    ] LOOP
        EXECUTE format('ALTER FUNCTION %s SET search_path TO %I, pg_catalog, pg_temp',
                   signature, current_schema);
    END LOOP;
END
$search_path_pins$;


--
-- Restore body validation for anything applied after this file.
--

RESET check_function_bodies;
