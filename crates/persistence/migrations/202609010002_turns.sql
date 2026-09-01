-- Turns and input: the turn lifecycle and its attempts, accepted input and
-- its content parts, the submit-input command, the queued-input origin, the
-- semantic transcript, per-turn resolved model settings, turn-scoped outbox
-- events, recovery origins for restarts and runner interrupts, and the
-- automatic reconciliation cursors that discover turns needing repair.

-- Function bodies may read tables a later section or file creates;
-- 202609010000_core.sql explains why validation is deferred.
SET check_function_bodies = false;

--
-- Functions.
--

--
-- Name: accepted_input_content_parts_json(uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION accepted_input_content_parts_json(checked_input uuid) RETURNS jsonb
    LANGUAGE sql STABLE
    AS $$
    SELECT COALESCE(
        jsonb_agg(
            jsonb_build_object(
                'position', position,
                'part_kind', part_kind,
                'text_value', text_value,
                'blob_digest', CASE
                    WHEN blob_digest IS NULL THEN NULL
                    ELSE 'sha256:' || encode(blob_digest, 'hex')
                END,
                'attachment_kind', attachment_kind,
                'declared_media_type', declared_media_type,
                'display_filename', display_filename
            ) ORDER BY position
        ),
        '[]'::jsonb
    )
      FROM accepted_input_content_part
     WHERE accepted_input_id = checked_input;
$$;


--
-- Name: accepted_input_parts_are_valid(uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION accepted_input_parts_are_valid(checked_input uuid) RETURNS boolean
    LANGUAGE sql STABLE
    AS $$
    SELECT count(*) BETWEEN 1 AND 256
       AND min(position) = 0
       AND max(position) = count(*) - 1
       AND COALESCE(sum(octet_length(convert_to(text_value, 'UTF8')))
            FILTER (WHERE part_kind = 'text'), 0) <= 1048576
       AND NOT EXISTS (
            SELECT 1
              FROM accepted_input_content_part AS current
              JOIN accepted_input_content_part AS prior
                ON prior.accepted_input_id = current.accepted_input_id
               AND prior.position + 1 = current.position
             WHERE current.accepted_input_id = checked_input
               AND current.part_kind = 'text'
               AND prior.part_kind = 'text')
      FROM accepted_input_content_part
     WHERE accepted_input_id = checked_input;
$$;


--
-- Name: accepted_input_parts_match_command(uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION accepted_input_parts_match_command(checked_input uuid) RETURNS boolean
    LANGUAGE sql STABLE
    AS $$
    SELECT accepting_command_id IS NULL OR (
        NOT EXISTS (
            SELECT position, part_kind, text_value, blob_digest,
                   attachment_kind, declared_media_type, display_filename
              FROM accepted_input_content_part
             WHERE accepted_input_id = checked_input
            EXCEPT
            SELECT position, part_kind, text_value, blob_digest,
                   attachment_kind, declared_media_type, display_filename
              FROM submit_input_command_content_part
             WHERE command_id = accepted.accepting_command_id)
        AND NOT EXISTS (
            SELECT position, part_kind, text_value, blob_digest,
                   attachment_kind, declared_media_type, display_filename
              FROM submit_input_command_content_part
             WHERE command_id = accepted.accepting_command_id
            EXCEPT
            SELECT position, part_kind, text_value, blob_digest,
                   attachment_kind, declared_media_type, display_filename
              FROM accepted_input_content_part
             WHERE accepted_input_id = checked_input)
    )
      FROM accepted_input AS accepted
     WHERE accepted.accepted_input_id = checked_input;
$$;


--
-- Name: accepted_input_projected_text(uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION accepted_input_projected_text(checked_input uuid) RETURNS text
    LANGUAGE sql STABLE STRICT PARALLEL SAFE
    AS $$
    SELECT string_agg(part.text_value, E'\n' ORDER BY part.position)
      FROM accepted_input_content_part AS part
     WHERE part.accepted_input_id = checked_input
       AND part.part_kind = 'text'
$$;


--
-- Name: accepted_input_turn_is_first_nonterminal(uuid, uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION accepted_input_turn_is_first_nonterminal(checked_session uuid, checked_turn uuid) RETURNS boolean
    LANGUAGE sql STABLE
    AS $$
    WITH RECURSIVE derived_order (
        turn_id,
        root_position,
        interrupt_depth
    ) AS (
        SELECT
            lifecycle.turn_id,
            lifecycle.acceptance_position,
            0::bigint
          FROM turn_lifecycle AS lifecycle
          LEFT JOIN queued_input_origin AS origin
            ON origin.turn_id = lifecycle.turn_id
           AND origin.session_id = lifecycle.session_id
         WHERE lifecycle.session_id = checked_session
           AND (
                origin.priority_kind = 'ordinary'
                OR (
                    lifecycle.origin_kind = 'delegation'
                    AND (
                        EXISTS (
                            SELECT 1
                              FROM session_delegation_initial_task AS task
                             WHERE task.turn_id = lifecycle.turn_id
                               AND task.child_session_id = lifecycle.session_id
                        )
                        OR EXISTS (
                            SELECT 1
                              FROM session_delegation_wake_turn_origin AS wake
                             WHERE wake.turn_id = lifecycle.turn_id
                               AND wake.recipient_session_id = lifecycle.session_id
                        )
                    )
                )
           )
           AND goal_turn_is_runtime_relevant(
                lifecycle.session_id,
                lifecycle.turn_id
           )
        UNION ALL
        SELECT
            successor.turn_id,
            predecessor.root_position,
            predecessor.interrupt_depth + 1
          FROM derived_order AS predecessor
          JOIN queued_input_origin AS successor
            ON successor.session_id = checked_session
           AND successor.priority_kind = 'interrupt_immediately_after'
           AND successor.interrupt_predecessor_turn_id = predecessor.turn_id
          JOIN turn_lifecycle AS successor_lifecycle
            ON successor_lifecycle.turn_id = successor.turn_id
           AND successor_lifecycle.session_id = successor.session_id
         WHERE goal_turn_is_runtime_relevant(
            successor_lifecycle.session_id,
            successor_lifecycle.turn_id
         )
    ),
    ranked AS (
        SELECT
            turn_id,
            row_number() OVER (
                ORDER BY root_position, interrupt_depth
            ) AS queue_rank
          FROM derived_order
    ),
    candidate AS (
        SELECT queue_rank
          FROM ranked
         WHERE turn_id = checked_turn
    )
    SELECT EXISTS (SELECT 1 FROM candidate)
       AND NOT EXISTS (
            SELECT 1
              FROM ranked AS earlier
              JOIN turn_lifecycle AS lifecycle
                ON lifecycle.turn_id = earlier.turn_id
               AND lifecycle.session_id = checked_session
              JOIN candidate
                ON earlier.queue_rank < candidate.queue_rank
             WHERE lifecycle.state_kind <> 'terminal'
       )
$$;


--
-- Name: accepted_input_turn_queue_predecessor(uuid, uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION accepted_input_turn_queue_predecessor(checked_session uuid, checked_turn uuid) RETURNS uuid
    LANGUAGE sql STABLE
    AS $$
    WITH RECURSIVE derived_order (
        turn_id,
        root_position,
        interrupt_depth
    ) AS (
        SELECT
            lifecycle.turn_id,
            lifecycle.acceptance_position,
            0::bigint
          FROM turn_lifecycle AS lifecycle
          LEFT JOIN queued_input_origin AS origin
            ON origin.turn_id = lifecycle.turn_id
           AND origin.session_id = lifecycle.session_id
         WHERE lifecycle.session_id = checked_session
           AND (
                origin.priority_kind = 'ordinary'
                OR (
                    lifecycle.origin_kind = 'delegation'
                    AND (
                        EXISTS (
                            SELECT 1
                              FROM session_delegation_initial_task AS task
                             WHERE task.turn_id = lifecycle.turn_id
                               AND task.child_session_id = lifecycle.session_id
                        )
                        OR EXISTS (
                            SELECT 1
                              FROM session_delegation_wake_turn_origin AS wake
                             WHERE wake.turn_id = lifecycle.turn_id
                               AND wake.recipient_session_id = lifecycle.session_id
                        )
                    )
                )
           )
           AND goal_turn_is_queue_order_relevant(
                lifecycle.session_id,
                lifecycle.turn_id
           )
        UNION ALL
        SELECT
            successor.turn_id,
            predecessor.root_position,
            predecessor.interrupt_depth + 1
          FROM derived_order AS predecessor
          JOIN queued_input_origin AS successor
            ON successor.session_id = checked_session
           AND successor.priority_kind = 'interrupt_immediately_after'
           AND successor.interrupt_predecessor_turn_id = predecessor.turn_id
          JOIN turn_lifecycle AS successor_lifecycle
            ON successor_lifecycle.turn_id = successor.turn_id
           AND successor_lifecycle.session_id = successor.session_id
         WHERE goal_turn_is_queue_order_relevant(
            successor_lifecycle.session_id,
            successor_lifecycle.turn_id
         )
    ),
    ranked AS (
        SELECT
            turn_id,
            lag(turn_id) OVER (
                ORDER BY root_position, interrupt_depth
            ) AS predecessor_turn
          FROM derived_order
    )
    SELECT predecessor_turn
      FROM ranked
     WHERE turn_id = checked_turn
$$;


--
-- Name: assert_cancelled_turn_final_state(uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION assert_cancelled_turn_final_state(checked_turn_id uuid) RETURNS void
    LANGUAGE plpgsql
    AS $$
DECLARE
    checked_session uuid;
    checked_starting_frontier uuid;
    checked_terminal_frontier uuid;
    checked_terminal_attempt uuid;
    checked_terminal_call uuid;
    runner_recovery_effect turn_runner_recovery_interrupt_effect%ROWTYPE;
    base_frontier uuid;
    base_member_count numeric(20, 0);
    terminal_member_count numeric(20, 0);
    prefix_mismatch_count bigint;
    checked_cancellation_entry uuid;
    cancellation_entry_count bigint;
    runner_tool_result_count bigint := 0;
    malformed_runner_result_count bigint := 0;
    contradictory_entry_count bigint;
    call_count bigint;
    outbox_count bigint;
BEGIN
    SELECT
        session_id,
        starting_frontier_id,
        terminal_frontier_id,
        terminal_attempt_id,
        terminal_model_call_id
      INTO
        checked_session,
        checked_starting_frontier,
        checked_terminal_frontier,
        checked_terminal_attempt,
        checked_terminal_call
      FROM turn_lifecycle
     WHERE turn_id = checked_turn_id
       AND state_kind = 'terminal'
       AND terminal_disposition_kind = 'cancelled';

    IF NOT FOUND THEN
        RETURN;
    END IF;

    PERFORM assert_terminal_started_turn_common_final_state(checked_turn_id);

    SELECT * INTO runner_recovery_effect
      FROM turn_runner_recovery_interrupt_effect
     WHERE session_id = checked_session
       AND turn_id = checked_turn_id;
    IF FOUND THEN
        IF checked_terminal_attempt IS DISTINCT FROM
                runner_recovery_effect.yielded_turn_attempt_id
           OR checked_terminal_call IS NOT NULL
           OR NOT EXISTS (
                SELECT 1
                  FROM turn_attempt
                 WHERE turn_attempt_id = checked_terminal_attempt
                   AND turn_id = checked_turn_id
                   AND session_id = checked_session
                   AND state_kind = 'ended'
                   AND end_variant = 'without_stop'
                   AND end_disposition = 'yielded_to_durable_wait'
                   AND interrupt_command_id IS NULL
                   AND interrupt_predecessor_turn_id IS NULL
           )
           OR (
                runner_recovery_effect.interrupted_tool_attempt_id IS NOT NULL
                AND NOT EXISTS (
                    SELECT 1
                      FROM tool_attempt AS stopped_tool
                     WHERE stopped_tool.attempt_id =
                            runner_recovery_effect.interrupted_tool_attempt_id
                       AND stopped_tool.session_id = checked_session
                       AND stopped_tool.turn_id = checked_turn_id
                       AND stopped_tool.state_kind = 'terminal'
                       AND stopped_tool.terminal_disposition_kind =
                            'known_failed'
                       AND stopped_tool.error_kind = 'crash_lost'
                       AND stopped_tool.error_detail IS NULL
                )
           )
        THEN
            RAISE EXCEPTION
                'runner recovery cancellation lacks its yielded attempt'
                USING ERRCODE = '23514';
        END IF;
        base_frontier := runner_recovery_effect.source_frontier_id;
        SELECT count(*)
          INTO runner_tool_result_count
          FROM tool_round AS round
          JOIN tool_request AS request
            ON request.producing_model_call_id = round.producing_model_call_id
           AND request.turn_id = round.turn_id
           AND request.session_id = round.session_id
         WHERE round.turn_id = checked_turn_id
           AND round.session_id = checked_session
           AND round.boundary_kind = 'continuing'
           AND round.boundary_frontier_id = base_frontier;
    ELSE
        IF NOT EXISTS (
            SELECT 1
              FROM turn_attempt
             WHERE turn_attempt_id = checked_terminal_attempt
               AND turn_id = checked_turn_id
               AND session_id = checked_session
               AND state_kind = 'ended'
               AND end_variant = 'after_cancellation'
               AND end_disposition = 'cancelled'
        ) THEN
            RAISE EXCEPTION 'cancelled turn lacks its exact ended attempt'
                USING ERRCODE = '23514';
        END IF;
        PERFORM assert_interrupt_attempt_proof(checked_terminal_attempt);
    END IF;

    SELECT count(*)
      INTO call_count
      FROM model_call
     WHERE turn_id = checked_turn_id
       AND session_id = checked_session;

    IF runner_recovery_effect.command_id IS NOT NULL THEN
        NULL;
    ELSIF checked_terminal_call IS NULL THEN
        IF call_count <> 0 THEN
            RAISE EXCEPTION 'directly cancelled turn names no call but stores one'
                USING ERRCODE = '23514';
        END IF;
        base_frontier := checked_starting_frontier;
    ELSE
        IF call_count <> 1
           OR NOT EXISTS (
                SELECT 1
                  FROM model_call
                 WHERE model_call_id = checked_terminal_call
                   AND turn_attempt_id = checked_terminal_attempt
                   AND turn_id = checked_turn_id
                   AND session_id = checked_session
                   AND state_kind = 'terminal'
                   AND terminal_disposition_kind = 'cancelled'
           )
        THEN
            RAISE EXCEPTION 'cancelled turn lacks its exact cancelled call'
                USING ERRCODE = '23514';
        END IF;
        SELECT context_frontier_id
          INTO base_frontier
          FROM model_call
         WHERE model_call_id = checked_terminal_call;
        PERFORM assert_model_call_final_state(checked_terminal_call);
    END IF;

    SELECT count(*)
      INTO cancellation_entry_count
      FROM semantic_transcript_entry
     WHERE source_session_id = checked_session
       AND payload_kind = 'turn_cancelled'
       AND cancelled_turn_id = checked_turn_id;
    SELECT semantic_entry_id
      INTO checked_cancellation_entry
      FROM semantic_transcript_entry
     WHERE source_session_id = checked_session
       AND payload_kind = 'turn_cancelled'
       AND cancelled_turn_id = checked_turn_id
     ORDER BY semantic_entry_id
     LIMIT 1;

    SELECT count(*)
      INTO contradictory_entry_count
      FROM semantic_transcript_entry
     WHERE source_session_id = checked_session
       AND (
            failed_turn_id = checked_turn_id
            OR completed_turn_id = checked_turn_id
            OR producing_model_call_id = checked_terminal_call
       )
       AND payload_kind IN (
            'turn_failed',
            'turn_completed',
            'assistant_text'
       );

    SELECT member_count
      INTO base_member_count
      FROM context_frontier
     WHERE owning_session_id = checked_session
       AND context_frontier_id = base_frontier;
    SELECT member_count
      INTO terminal_member_count
      FROM context_frontier
     WHERE owning_session_id = checked_session
       AND context_frontier_id = checked_terminal_frontier;

    IF runner_recovery_effect.command_id IS NOT NULL THEN
        SELECT count(*)
          INTO malformed_runner_result_count
          FROM tool_round AS round
          JOIN generate_series(0, round.request_count - 1)
            AS expected(request_ordinal) ON true
          JOIN tool_request AS request
            ON request.producing_model_call_id = round.producing_model_call_id
           AND request.session_id = round.session_id
           AND request.turn_id = round.turn_id
           AND request.request_ordinal = expected.request_ordinal
          LEFT JOIN context_frontier_member AS member
            ON member.owning_session_id = checked_session
           AND member.context_frontier_id = checked_terminal_frontier
           AND member.member_position =
                base_member_count + expected.request_ordinal + 1
          LEFT JOIN semantic_transcript_entry AS entry
            ON entry.source_session_id = member.source_session_id
           AND entry.semantic_entry_id = member.semantic_entry_id
          LEFT JOIN tool_attempt AS attempt
            ON attempt.attempt_id = entry.tool_result_attempt_id
         WHERE round.session_id = checked_session
           AND round.turn_id = checked_turn_id
           AND round.boundary_kind = 'continuing'
           AND round.boundary_frontier_id = base_frontier
           AND (
                member.source_session_id IS DISTINCT FROM checked_session
                OR ((
                    entry.payload_kind = 'tool_execution_result'
                    AND attempt.request_id = request.request_id
                )
                OR (
                    entry.payload_kind IN ('tool_denied', 'tool_closed_by_turn_end')
                    AND entry.tool_result_request_id = request.request_id
                )) IS NOT TRUE
           );
    END IF;

    SELECT count(*)
      INTO prefix_mismatch_count
      FROM context_frontier_member AS base_member
      LEFT JOIN context_frontier_member AS terminal_member
        ON terminal_member.owning_session_id = base_member.owning_session_id
       AND terminal_member.context_frontier_id = checked_terminal_frontier
       AND terminal_member.member_position = base_member.member_position
       AND terminal_member.source_session_id = base_member.source_session_id
       AND terminal_member.semantic_entry_id = base_member.semantic_entry_id
     WHERE base_member.owning_session_id = checked_session
       AND base_member.context_frontier_id = base_frontier
       AND terminal_member.member_position IS NULL;

    SELECT count(*)
      INTO outbox_count
      FROM turn_cancelled_outbox_event
     WHERE session_id = checked_session
       AND turn_id = checked_turn_id
       AND cancellation_entry_id = checked_cancellation_entry
       AND terminal_frontier_id = checked_terminal_frontier;

    IF cancellation_entry_count <> 1
       OR contradictory_entry_count <> 0
       OR base_member_count IS NULL
       OR terminal_member_count IS DISTINCT FROM
            base_member_count + runner_tool_result_count + 1
       OR prefix_mismatch_count <> 0
       OR malformed_runner_result_count <> 0
       OR NOT EXISTS (
            SELECT 1
              FROM context_frontier_member
             WHERE owning_session_id = checked_session
               AND context_frontier_id = checked_terminal_frontier
               AND member_position = terminal_member_count
               AND source_session_id = checked_session
               AND semantic_entry_id = checked_cancellation_entry
       )
       OR outbox_count <> 1
    THEN
        RAISE EXCEPTION
            'cancelled turn lacks its exact semantic, frontier, or outbox boundary'
            USING ERRCODE = '23514';
    END IF;
END;
$$;


--
-- Name: assert_failed_terminal_execution_final_state(uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION assert_failed_terminal_execution_final_state(checked_turn_id uuid) RETURNS void
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF EXISTS (
        SELECT 1
          FROM tool_continuation_context_headroom AS headroom
          JOIN turn_lifecycle AS lifecycle
            ON lifecycle.turn_id = headroom.turn_id
           AND lifecycle.session_id = headroom.session_id
          JOIN turn_attempt AS attempt
            ON attempt.turn_attempt_id = headroom.terminal_attempt_id
           AND attempt.turn_id = headroom.turn_id
           AND attempt.session_id = headroom.session_id
         WHERE headroom.turn_id = checked_turn_id
           AND lifecycle.state_kind = 'terminal'
           AND lifecycle.terminal_disposition_kind = 'failed'
           AND lifecycle.terminal_attempt_id = headroom.terminal_attempt_id
           AND lifecycle.terminal_model_call_id IS NULL
           AND attempt.state_kind = 'ended'
           AND attempt.end_variant = 'without_stop'
           AND attempt.end_disposition = 'known_failure'
    ) THEN
        RETURN;
    END IF;

    PERFORM assert_failed_terminal_execution_before_context_headroom(
        checked_turn_id
    );
END;
$$;


--
-- Name: assert_failed_terminal_execution_without_cancellation(uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION assert_failed_terminal_execution_without_cancellation(checked_turn_id uuid) RETURNS void
    LANGUAGE plpgsql
    AS $$
DECLARE
    lifecycle turn_lifecycle%ROWTYPE;
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM tool_round
         WHERE turn_id = checked_turn_id
    ) THEN
        PERFORM assert_failed_terminal_execution_without_tool_loop(
            checked_turn_id
        );
        RETURN;
    END IF;

    SELECT *
      INTO lifecycle
      FROM turn_lifecycle
     WHERE turn_id = checked_turn_id
       AND state_kind = 'terminal'
       AND terminal_disposition_kind = 'failed';
    IF NOT FOUND THEN
        RETURN;
    END IF;

    IF lifecycle.terminal_attempt_id IS NULL
       OR NOT EXISTS (
            SELECT 1
              FROM turn_attempt
             WHERE turn_attempt_id = lifecycle.terminal_attempt_id
               AND turn_id = lifecycle.turn_id
               AND session_id = lifecycle.session_id
               AND state_kind = 'ended'
               AND end_variant = 'without_stop'
               AND end_disposition IN ('known_failure', 'lost')
       )
       OR EXISTS (
            SELECT 1
              FROM turn_attempt
             WHERE turn_id = lifecycle.turn_id
               AND session_id = lifecycle.session_id
               AND turn_attempt_id <> lifecycle.terminal_attempt_id
               AND (
                    state_kind <> 'ended'
                    OR end_variant <> 'without_stop'
                    OR end_disposition <> 'yielded_to_durable_wait'
               )
       )
    THEN
        RAISE EXCEPTION
            'failed tool-loop turn % lacks its exact linear ended attempt',
            checked_turn_id
            USING ERRCODE = '23514';
    END IF;

    IF lifecycle.terminal_model_call_id IS NOT NULL THEN
        IF NOT EXISTS (
            SELECT 1
              FROM model_call
             WHERE model_call_id = lifecycle.terminal_model_call_id
               AND turn_attempt_id = lifecycle.terminal_attempt_id
               AND turn_id = lifecycle.turn_id
               AND session_id = lifecycle.session_id
               AND state_kind = 'terminal'
               AND terminal_disposition_kind IN ('known_failed', 'cancelled')
        ) THEN
            RAISE EXCEPTION
                'failed tool-loop turn % lacks its exact terminal call',
                checked_turn_id
                USING ERRCODE = '23514';
        END IF;
        PERFORM assert_model_call_final_state(
            lifecycle.terminal_model_call_id
        );
    ELSIF NOT EXISTS (
        SELECT 1
          FROM tool_attempt
         WHERE issuing_turn_attempt_id = lifecycle.terminal_attempt_id
           AND turn_id = lifecycle.turn_id
           AND session_id = lifecycle.session_id
           AND state_kind = 'terminal'
           AND terminal_disposition_kind = 'known_failed'
           AND error_kind = 'crash_lost'
    ) AND NOT EXISTS (
        SELECT 1
          FROM repo_watch_headless_approval_escalation AS escalation
          JOIN tool_approval_judge_model_call AS judge
            ON judge.model_call_id = escalation.model_call_id
           AND judge.session_id = escalation.session_id
           AND judge.turn_id = escalation.turn_id
           AND judge.request_id = escalation.request_id
         WHERE escalation.turn_id = lifecycle.turn_id
           AND escalation.session_id = lifecycle.session_id
           AND escalation.terminal_attempt_id = lifecycle.terminal_attempt_id
           AND judge.state_kind = 'terminal'
           AND judge.terminal_disposition_kind = 'completed'
           AND judge.recommendation_kind = 'escalate_to_human'
    ) AND NOT EXISTS (
        SELECT 1
          FROM commissioned_dispatch_headless_approval_escalation AS escalation
          JOIN tool_approval_judge_model_call AS judge
            ON judge.model_call_id = escalation.model_call_id
           AND judge.session_id = escalation.session_id
           AND judge.turn_id = escalation.turn_id
           AND judge.request_id = escalation.request_id
         WHERE escalation.turn_id = lifecycle.turn_id
           AND escalation.session_id = lifecycle.session_id
           AND escalation.terminal_attempt_id = lifecycle.terminal_attempt_id
           AND judge.state_kind = 'terminal'
           AND judge.terminal_disposition_kind = 'completed'
           AND judge.recommendation_kind = 'escalate_to_human'
    ) THEN
        RAISE EXCEPTION
            'failed tool-loop turn % lacks its exact terminal execution cause',
            checked_turn_id
            USING ERRCODE = '23514';
    END IF;
END;
$$;


--
-- Name: assert_interrupt_attempt_proof(uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION assert_interrupt_attempt_proof(checked_attempt_id uuid) RETURNS void
    LANGUAGE plpgsql
    AS $$
DECLARE
    checked_turn uuid;
    checked_session uuid;
    checked_command uuid;
    matching_records bigint;
BEGIN
    SELECT
        turn_id,
        session_id,
        interrupt_command_id
      INTO
        checked_turn,
        checked_session,
        checked_command
      FROM turn_attempt
     WHERE turn_attempt_id = checked_attempt_id
       AND interrupt_command_id IS NOT NULL;

    IF NOT FOUND THEN
        RETURN;
    END IF;

    SELECT count(*)
      INTO matching_records
      FROM submit_input_command AS command
      JOIN accepted_input AS accepted
        ON accepted.accepting_command_id = command.command_id
       AND accepted.accepted_input_id = command.result_accepted_input_id
       AND accepted.session_id = command.result_session_id
       AND accepted.origin_turn_id = command.result_turn_id
       AND accepted.delivery_kind = 'interrupt'
       AND accepted.expected_active_turn_id = checked_turn
       AND accepted.disposition_kind = 'origin_of'
      JOIN queued_input_origin AS successor
        ON successor.accepted_input_id = accepted.accepted_input_id
       AND successor.turn_id = accepted.origin_turn_id
       AND successor.session_id = accepted.session_id
       AND successor.priority_kind = 'interrupt_immediately_after'
       AND successor.interrupt_predecessor_turn_id = checked_turn
     WHERE command.command_id = checked_command
       AND command.result_kind = 'applied'
       AND command.rejection_kind IS NULL
       AND command.delivery_kind = 'interrupt'
       AND command.session_id = checked_session
       AND command.expected_active_turn_id = checked_turn;

    IF matching_records <> 1 THEN
        RAISE EXCEPTION 'turn attempt % lacks its exact applied interrupt proof',
            checked_attempt_id
            USING
                ERRCODE = '23503',
                CONSTRAINT = 'turn_attempt_interrupt_proof';
    END IF;
END;
$$;


--
-- Name: assert_reconciliation_required_turn_final_state(uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION assert_reconciliation_required_turn_final_state(checked_turn_id uuid) RETURNS void
    LANGUAGE plpgsql
    AS $$
DECLARE
    checked_session uuid;
    checked_attempt uuid;
    checked_call uuid;
    checked_tool_attempt uuid;
    checked_terminal_frontier uuid;
    source_frontier uuid;
    source_tool_round uuid;
    source_frontier_count bigint;
    member_mismatch_count bigint;
    contradictory_entry_count bigint;
    outbox_count bigint;
    interrupt_command uuid;
    interrupt_record_count bigint;
BEGIN
    SELECT
        session_id,
        terminal_attempt_id,
        terminal_model_call_id,
        terminal_tool_attempt_id,
        terminal_frontier_id
      INTO
        checked_session,
        checked_attempt,
        checked_call,
        checked_tool_attempt,
        checked_terminal_frontier
      FROM turn_lifecycle
     WHERE turn_id = checked_turn_id
       AND state_kind = 'terminal'
       AND terminal_disposition_kind = 'reconciliation_required';

    IF NOT FOUND THEN
        RETURN;
    END IF;

    PERFORM assert_terminal_started_turn_common_final_state(checked_turn_id);

    IF NOT EXISTS (
        SELECT 1
          FROM turn_attempt
            AS turn_attempt
         WHERE turn_attempt_id = checked_attempt
           AND turn_id = checked_turn_id
           AND session_id = checked_session
           AND state_kind = 'ended'
           AND (
                end_disposition IN ('ambiguous', 'lost')
                OR (
                    end_disposition = 'yielded_to_durable_wait'
                    AND EXISTS (
                        SELECT 1
                          FROM turn_runner_recovery_interrupt_effect AS effect
                         WHERE effect.session_id = checked_session
                           AND effect.turn_id = checked_turn_id
                           AND effect.yielded_turn_attempt_id =
                                turn_attempt.turn_attempt_id
                           AND effect.interrupted_tool_attempt_id =
                                checked_tool_attempt
                    )
                )
           )
           AND (
                (
                    end_variant = 'after_cancellation'
                    AND interrupt_command_id IS NOT NULL
                    AND interrupt_predecessor_turn_id = checked_turn_id
                )
                OR (
                    end_variant = 'without_stop'
                    AND interrupt_command_id IS NULL
                    AND interrupt_predecessor_turn_id IS NULL
                )
           )
    ) THEN
        RAISE EXCEPTION
            'reconciliation-required turn lacks exact ambiguous attempt'
            USING ERRCODE = '23514';
    END IF;

    SELECT interrupt_command_id
      INTO interrupt_command
      FROM turn_attempt
     WHERE turn_attempt_id = checked_attempt;
    IF interrupt_command IS NOT NULL THEN
        PERFORM assert_interrupt_attempt_proof(checked_attempt);
    END IF;

    SELECT count(*)
      INTO interrupt_record_count
      FROM submit_input_command AS command
      JOIN accepted_input AS accepted
        ON accepted.accepting_command_id = command.command_id
       AND accepted.accepted_input_id = command.result_accepted_input_id
       AND accepted.session_id = command.result_session_id
       AND accepted.origin_turn_id = command.result_turn_id
      JOIN queued_input_origin AS successor
        ON successor.accepted_input_id = accepted.accepted_input_id
       AND successor.turn_id = accepted.origin_turn_id
       AND successor.session_id = accepted.session_id
       AND successor.priority_kind = 'interrupt_immediately_after'
       AND successor.interrupt_predecessor_turn_id = checked_turn_id
     WHERE command.session_id = checked_session
       AND command.delivery_kind = 'interrupt'
       AND command.expected_active_turn_id = checked_turn_id
       AND command.result_kind = 'applied'
       AND command.rejection_kind IS NULL
       AND accepted.disposition_kind = 'origin_of'
       AND (
            interrupt_command IS NULL
            OR command.command_id = interrupt_command
       );
    IF interrupt_record_count = 0
       AND NOT EXISTS (
            SELECT 1
              FROM automatic_reconciliation AS recovery
             WHERE recovery.turn_id = checked_turn_id
               AND recovery.session_id = checked_session
               AND recovery.model_call_id IS NOT DISTINCT FROM checked_call
               AND recovery.tool_attempt_id IS NOT DISTINCT FROM
                   checked_tool_attempt
               AND recovery.state_kind = 'reconciled'
               AND recovery.attempt_count BETWEEN 1 AND 5
       )
    THEN
        RAISE EXCEPTION
            'reconciliation-required turn lacks exact automatic recovery authority'
            USING ERRCODE = '23514';
    END IF;
    IF interrupt_record_count > 1 THEN
        RAISE EXCEPTION
            'reconciliation-required turn has more than one applied interrupt'
            USING ERRCODE = '23514';
    END IF;

    IF checked_call IS NOT NULL THEN
        SELECT context_frontier_id
          INTO source_frontier
          FROM model_call
         WHERE model_call_id = checked_call
           AND turn_attempt_id = checked_attempt
           AND turn_id = checked_turn_id
           AND session_id = checked_session
           AND state_kind = 'terminal'
           AND terminal_disposition_kind = 'ambiguous';
        IF NOT FOUND THEN
            RAISE EXCEPTION
                'reconciliation-required turn lacks exact ambiguous call'
                USING ERRCODE = '23514';
        END IF;
        PERFORM assert_model_call_final_state(checked_call);
    ELSE
        SELECT
            round.boundary_frontier_id,
            round.producing_model_call_id
          INTO
            source_frontier,
            source_tool_round
          FROM tool_attempt AS attempt
          JOIN tool_request AS request
            ON request.request_id = attempt.request_id
          JOIN tool_round AS round
            ON round.producing_model_call_id =
               request.producing_model_call_id
           AND round.turn_id = request.turn_id
           AND round.session_id = request.session_id
         WHERE attempt.attempt_id = checked_tool_attempt
           AND attempt.issuing_turn_attempt_id = checked_attempt
           AND attempt.turn_id = checked_turn_id
           AND attempt.session_id = checked_session
           AND attempt.state_kind = 'terminal'
           AND attempt.terminal_disposition_kind = 'ambiguous'
           AND round.boundary_kind = 'continuing';
        IF NOT FOUND THEN
            RAISE EXCEPTION
                'reconciliation-required turn lacks exact ambiguous tool attempt'
                USING ERRCODE = '23514';
        END IF;
    END IF;

    IF checked_call IS NOT NULL THEN
        SELECT count(*)
          INTO member_mismatch_count
          FROM (
                (
                    SELECT
                        member_position,
                        source_session_id,
                        semantic_entry_id
                      FROM context_frontier_member
                     WHERE owning_session_id = checked_session
                       AND context_frontier_id = source_frontier
                    EXCEPT
                    SELECT
                        member_position,
                        source_session_id,
                        semantic_entry_id
                      FROM context_frontier_member
                     WHERE owning_session_id = checked_session
                       AND context_frontier_id =
                           checked_terminal_frontier
                )
                UNION ALL
                (
                    SELECT
                        member_position,
                        source_session_id,
                        semantic_entry_id
                      FROM context_frontier_member
                     WHERE owning_session_id = checked_session
                       AND context_frontier_id =
                           checked_terminal_frontier
                    EXCEPT
                    SELECT
                        member_position,
                        source_session_id,
                        semantic_entry_id
                      FROM context_frontier_member
                     WHERE owning_session_id = checked_session
                       AND context_frontier_id = source_frontier
                )
          ) AS mismatch;
    ELSE
        SELECT count(*)
          INTO source_frontier_count
          FROM context_frontier_member
         WHERE owning_session_id = checked_session
           AND context_frontier_id = source_frontier;

        WITH expected_member AS (
            SELECT
                member_position,
                source_session_id,
                semantic_entry_id
              FROM context_frontier_member
             WHERE owning_session_id = checked_session
               AND context_frontier_id = source_frontier
            UNION ALL
            SELECT
                source_frontier_count + request.request_ordinal + 1,
                result.source_session_id,
                result.semantic_entry_id
              FROM tool_request AS request
              JOIN semantic_transcript_entry AS result
                ON result.source_session_id = checked_session
               AND result.payload_kind IN (
                    'tool_execution_result',
                    'tool_denied',
                    'tool_closed_by_turn_end',
                    'delegation_result'
               )
              LEFT JOIN tool_attempt AS result_attempt
                ON result_attempt.attempt_id =
                   result.tool_result_attempt_id
             WHERE request.producing_model_call_id = source_tool_round
               AND (
                    result.tool_result_request_id = request.request_id
                    OR result_attempt.request_id = request.request_id
               )
        )
        SELECT count(*)
          INTO member_mismatch_count
          FROM (
                (
                    SELECT *
                      FROM expected_member
                    EXCEPT
                    SELECT
                        member_position,
                        source_session_id,
                        semantic_entry_id
                      FROM context_frontier_member
                     WHERE owning_session_id = checked_session
                       AND context_frontier_id =
                           checked_terminal_frontier
                )
                UNION ALL
                (
                    SELECT
                        member_position,
                        source_session_id,
                        semantic_entry_id
                      FROM context_frontier_member
                     WHERE owning_session_id = checked_session
                       AND context_frontier_id =
                           checked_terminal_frontier
                    EXCEPT
                    SELECT *
                      FROM expected_member
                )
          ) AS mismatch;
    END IF;

    SELECT count(*)
      INTO contradictory_entry_count
      FROM semantic_transcript_entry
     WHERE source_session_id = checked_session
       AND (
            failed_turn_id = checked_turn_id
            OR completed_turn_id = checked_turn_id
            OR cancelled_turn_id = checked_turn_id
            OR (
                checked_call IS NOT NULL
                AND producing_model_call_id = checked_call
            )
       )
       AND payload_kind IN (
            'turn_failed',
            'turn_completed',
            'turn_cancelled',
            'assistant_text'
       );

    SELECT count(*)
      INTO outbox_count
      FROM turn_reconciliation_required_outbox_event
     WHERE session_id = checked_session
       AND turn_id = checked_turn_id
       AND model_call_id IS NOT DISTINCT FROM checked_call
       AND tool_attempt_id IS NOT DISTINCT FROM checked_tool_attempt
       AND terminal_frontier_id = checked_terminal_frontier;

    IF member_mismatch_count <> 0
       OR contradictory_entry_count <> 0
       OR outbox_count <> 1
    THEN
        RAISE EXCEPTION
            'reconciliation-required turn lacks exact frontier or outbox boundary'
            USING ERRCODE = '23514';
    END IF;
END;
$$;


--
-- Name: assert_steering_accepted_input_final_state(uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION assert_steering_accepted_input_final_state(checked_accepted_input_id uuid) RETURNS void
    LANGUAGE plpgsql
    AS $$
DECLARE
    checked_disposition text;
    checked_call uuid;
    steering_entry_count bigint;
BEGIN
    SELECT disposition_kind, consuming_model_call_id
      INTO checked_disposition, checked_call
      FROM accepted_input
     WHERE accepted_input_id = checked_accepted_input_id;

    IF NOT FOUND THEN
        RETURN;
    END IF;

    SELECT count(*)
      INTO steering_entry_count
      FROM semantic_transcript_entry
     WHERE origin_accepted_input_id = checked_accepted_input_id
       AND payload_kind = 'steering_accepted_input';

    IF checked_disposition = 'consumed_as_steering' THEN
        IF checked_call IS NULL OR steering_entry_count <> 1 THEN
            RAISE EXCEPTION 'consumed steering requires one exact semantic entry and call'
                USING ERRCODE = '23514';
        END IF;
        PERFORM assert_model_call_steering_final_state(checked_call);
    ELSIF steering_entry_count <> 0 OR checked_call IS NOT NULL THEN
        RAISE EXCEPTION 'unconsumed input cannot carry steering-consumption effects'
            USING ERRCODE = '23514';
    END IF;
END;
$$;


--
-- Name: assert_steering_turn_terminal_final_state(uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION assert_steering_turn_terminal_final_state(checked_turn_id uuid) RETURNS void
    LANGUAGE plpgsql
    AS $$
DECLARE
    checked_session uuid;
    checked_starting_frontier uuid;
    checked_terminal_frontier uuid;
    checked_terminal_attempt uuid;
    checked_terminal_call uuid;
    checked_turn_disposition text;
    checked_call_frontier uuid;
    checked_call_disposition text;
    checked_attempt_disposition text;
    call_member_count numeric(20, 0);
    terminal_member_count numeric(20, 0);
    prefix_mismatch_count bigint;
    failure_entry_count bigint;
    failure_entry_id uuid;
    completion_entry_count bigint;
    completion_entry_id uuid;
    assistant_entry_count bigint;
    assistant_member_count bigint;
BEGIN
    SELECT
        lifecycle.session_id,
        lifecycle.starting_frontier_id,
        lifecycle.terminal_frontier_id,
        lifecycle.terminal_attempt_id,
        lifecycle.terminal_model_call_id,
        lifecycle.terminal_disposition_kind,
        call.context_frontier_id,
        call.terminal_disposition_kind,
        attempt.end_disposition
      INTO
        checked_session,
        checked_starting_frontier,
        checked_terminal_frontier,
        checked_terminal_attempt,
        checked_terminal_call,
        checked_turn_disposition,
        checked_call_frontier,
        checked_call_disposition,
        checked_attempt_disposition
      FROM turn_lifecycle AS lifecycle
      JOIN model_call AS call
        ON call.model_call_id = lifecycle.terminal_model_call_id
       AND call.turn_id = lifecycle.turn_id
       AND call.session_id = lifecycle.session_id
      JOIN turn_attempt AS attempt
        ON attempt.turn_attempt_id = lifecycle.terminal_attempt_id
       AND attempt.turn_id = lifecycle.turn_id
       AND attempt.session_id = lifecycle.session_id
     WHERE lifecycle.turn_id = checked_turn_id
       AND lifecycle.state_kind = 'terminal'
       AND call.context_frontier_id <> lifecycle.starting_frontier_id
       AND call.state_kind = 'terminal'
       AND attempt.state_kind = 'ended';

    IF NOT FOUND THEN
        RAISE EXCEPTION 'steering terminal turn lacks its exact call and attempt'
            USING ERRCODE = '23514';
    END IF;

    PERFORM assert_model_call_final_state(checked_terminal_call);
    PERFORM assert_context_frontier_complete_membership(
        checked_session,
        checked_call_frontier
    );
    PERFORM assert_context_frontier_complete_membership(
        checked_session,
        checked_terminal_frontier
    );

    SELECT member_count
      INTO call_member_count
      FROM context_frontier
     WHERE owning_session_id = checked_session
       AND context_frontier_id = checked_call_frontier;
    SELECT member_count
      INTO terminal_member_count
      FROM context_frontier
     WHERE owning_session_id = checked_session
       AND context_frontier_id = checked_terminal_frontier;

    SELECT count(*)
      INTO prefix_mismatch_count
      FROM context_frontier_member AS call_member
      LEFT JOIN context_frontier_member AS terminal_member
        ON terminal_member.owning_session_id = call_member.owning_session_id
       AND terminal_member.context_frontier_id = checked_terminal_frontier
       AND terminal_member.member_position = call_member.member_position
     WHERE call_member.owning_session_id = checked_session
       AND call_member.context_frontier_id = checked_call_frontier
       AND ROW(
            terminal_member.source_session_id,
            terminal_member.semantic_entry_id
       ) IS DISTINCT FROM ROW(
            call_member.source_session_id,
            call_member.semantic_entry_id
       );

    SELECT count(*)
      INTO failure_entry_count
      FROM semantic_transcript_entry
     WHERE source_session_id = checked_session
       AND payload_kind = 'turn_failed'
       AND failed_turn_id = checked_turn_id;
    SELECT semantic_entry_id
      INTO failure_entry_id
      FROM semantic_transcript_entry
     WHERE source_session_id = checked_session
       AND payload_kind = 'turn_failed'
       AND failed_turn_id = checked_turn_id
     ORDER BY semantic_entry_id
     LIMIT 1;
    SELECT count(*)
      INTO completion_entry_count
      FROM semantic_transcript_entry
     WHERE source_session_id = checked_session
       AND payload_kind = 'turn_completed'
       AND completed_turn_id = checked_turn_id;
    SELECT semantic_entry_id
      INTO completion_entry_id
      FROM semantic_transcript_entry
     WHERE source_session_id = checked_session
       AND payload_kind = 'turn_completed'
       AND completed_turn_id = checked_turn_id
     ORDER BY semantic_entry_id
     LIMIT 1;
    SELECT count(*)
      INTO assistant_entry_count
      FROM semantic_transcript_entry
     WHERE source_session_id = checked_session
       AND payload_kind = 'assistant_text'
       AND producing_model_call_id = checked_terminal_call;
    SELECT count(*)
      INTO assistant_member_count
      FROM context_frontier_member AS member
      JOIN semantic_transcript_entry AS entry
        ON entry.source_session_id = member.source_session_id
       AND entry.semantic_entry_id = member.semantic_entry_id
     WHERE member.owning_session_id = checked_session
       AND member.context_frontier_id = checked_terminal_frontier
       AND member.member_position > call_member_count
       AND member.member_position < terminal_member_count
       AND entry.payload_kind = 'assistant_text'
       AND entry.producing_model_call_id = checked_terminal_call;

    IF prefix_mismatch_count <> 0 THEN
        RAISE EXCEPTION 'steering terminal frontier does not retain its call prefix'
            USING ERRCODE = '23514';
    END IF;

    IF checked_turn_disposition = 'completed' THEN
        IF checked_call_disposition IS DISTINCT FROM 'completed'
           OR checked_attempt_disposition NOT IN ('turn_completed', 'lost')
           OR failure_entry_count <> 0
           OR completion_entry_count <> 1
           OR terminal_member_count
                IS DISTINCT FROM call_member_count + assistant_entry_count + 1
           OR assistant_member_count <> assistant_entry_count
           OR NOT EXISTS (
                SELECT 1
                  FROM context_frontier_member
                 WHERE owning_session_id = checked_session
                   AND context_frontier_id = checked_terminal_frontier
                   AND member_position = terminal_member_count
                   AND source_session_id = checked_session
                   AND semantic_entry_id = completion_entry_id
           )
        THEN
            RAISE EXCEPTION 'completed steering turn lacks its ordered response boundary'
                USING ERRCODE = '23514';
        END IF;
    ELSIF checked_turn_disposition = 'refused' THEN
        IF checked_call_disposition IS DISTINCT FROM 'refused'
           OR checked_attempt_disposition NOT IN ('turn_refused', 'lost')
           OR checked_terminal_frontier = checked_call_frontier
           OR terminal_member_count IS DISTINCT FROM call_member_count
           OR failure_entry_count <> 0
           OR completion_entry_count <> 0
           OR assistant_entry_count <> 0
        THEN
            RAISE EXCEPTION 'refused steering turn lacks its equal-content boundary'
                USING ERRCODE = '23514';
        END IF;
    ELSIF checked_turn_disposition = 'failed' THEN
        IF checked_call_disposition NOT IN ('known_failed', 'cancelled')
           OR checked_attempt_disposition NOT IN ('known_failure', 'lost')
           OR failure_entry_count <> 1
           OR completion_entry_count <> 0
           OR assistant_entry_count <> 0
           OR terminal_member_count IS DISTINCT FROM call_member_count + 1
           OR NOT EXISTS (
                SELECT 1
                  FROM context_frontier_member
                 WHERE owning_session_id = checked_session
                   AND context_frontier_id = checked_terminal_frontier
                   AND member_position = terminal_member_count
                   AND source_session_id = checked_session
                   AND semantic_entry_id = failure_entry_id
           )
        THEN
            RAISE EXCEPTION 'failed steering turn lacks its exact failure boundary'
                USING ERRCODE = '23514';
        END IF;
    ELSE
        RAISE EXCEPTION 'unsupported steering terminal disposition %', checked_turn_disposition
            USING ERRCODE = '23514';
    END IF;
END;
$$;


--
-- Name: assert_terminal_started_turn_common_final_state(uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION assert_terminal_started_turn_common_final_state(checked_turn_id uuid) RETURNS void
    LANGUAGE plpgsql
    AS $$
DECLARE
    checked_session uuid;
    checked_origin_input uuid;
    checked_position numeric(20, 0);
    checked_attempt_history boolean;
    checked_lineage text;
    checked_predecessor uuid;
    checked_starting_frontier uuid;
    checked_terminal_attempt uuid;
    attempt_count bigint;
    ended_attempt_count bigint;
    origin_entry_count bigint;
    origin_entry uuid;
    starting_member_count numeric(20, 0);
    origin_member_count bigint;
    origin_member_position numeric(20, 0);
    predecessor_turn uuid;
    predecessor_frontier uuid;
    predecessor_member_count numeric(20, 0);
    prefix_mismatch_count bigint;
BEGIN
    SELECT
        session_id,
        origin_accepted_input_id,
        acceptance_position,
        attempt_history_present,
        start_lineage_kind,
        immediate_predecessor_turn_id,
        starting_frontier_id,
        terminal_attempt_id
      INTO
        checked_session,
        checked_origin_input,
        checked_position,
        checked_attempt_history,
        checked_lineage,
        checked_predecessor,
        checked_starting_frontier,
        checked_terminal_attempt
      FROM turn_lifecycle
     WHERE turn_id = checked_turn_id
       AND state_kind = 'terminal';

    IF NOT FOUND THEN
        RETURN;
    END IF;

    SELECT
        count(*),
        count(*) FILTER (
            WHERE state_kind = 'ended'
              AND turn_attempt_id = checked_terminal_attempt
        )
      INTO attempt_count, ended_attempt_count
      FROM turn_attempt
     WHERE turn_id = checked_turn_id
       AND session_id = checked_session;

    IF checked_attempt_history IS DISTINCT FROM (attempt_count > 0)
       OR attempt_count <> 1
       OR ended_attempt_count <> 1
    THEN
        RAISE EXCEPTION
            'terminal turn % lacks its exact single ended attempt history',
            checked_turn_id
            USING ERRCODE = '23514';
    END IF;

    SELECT count(*)
      INTO origin_entry_count
      FROM turn_lifecycle_origin_semantic_entry(
            checked_turn_id,
            checked_session,
            checked_origin_input
      );
    IF origin_entry_count <> 1 THEN
        RAISE EXCEPTION
            'terminal turn % lacks its exact origin entry',
            checked_turn_id
            USING ERRCODE = '23514';
    END IF;
    SELECT semantic_entry_id
      INTO origin_entry
      FROM turn_lifecycle_origin_semantic_entry(
            checked_turn_id,
            checked_session,
            checked_origin_input
      );

    SELECT member_count
      INTO starting_member_count
      FROM context_frontier
     WHERE owning_session_id = checked_session
       AND context_frontier_id = checked_starting_frontier;
    SELECT count(*), max(member_position)
      INTO origin_member_count, origin_member_position
      FROM context_frontier_member
     WHERE owning_session_id = checked_session
       AND context_frontier_id = checked_starting_frontier
       AND source_session_id = checked_session
       AND semantic_entry_id = origin_entry;
    IF starting_member_count IS NULL
       OR origin_member_count <> 1
       OR origin_member_position IS DISTINCT FROM starting_member_count
       OR NOT turn_start_model_identity_boundary_is_valid(
            checked_turn_id,
            checked_starting_frontier
       )
    THEN
        RAISE EXCEPTION
            'terminal turn % starting frontier lacks its final origin',
            checked_turn_id
            USING ERRCODE = '23514';
    END IF;

    predecessor_turn := accepted_input_turn_queue_predecessor(
        checked_session,
        checked_turn_id
    );
    IF checked_lineage = 'first_in_session' THEN
        IF checked_predecessor IS NOT NULL
           OR predecessor_turn IS NOT NULL
           OR NOT first_native_starting_frontier_matches_seed(
                checked_session,
                checked_starting_frontier
            )
        THEN
            RAISE EXCEPTION
                'terminal turn % has inconsistent first lineage',
                checked_turn_id
                USING ERRCODE = '23514';
        END IF;
    ELSIF checked_lineage = 'after' THEN
        IF checked_predecessor IS DISTINCT FROM predecessor_turn THEN
            RAISE EXCEPTION
                'terminal turn % does not name its queue predecessor',
                checked_turn_id
                USING ERRCODE = '23514';
        END IF;
        SELECT turn_lifecycle_effective_terminal_frontier(
                    checked_session, checked_predecessor
               )
          INTO predecessor_frontier;
        IF predecessor_frontier IS NULL THEN
            RAISE EXCEPTION
                'terminal turn % predecessor is not terminal',
                checked_turn_id
                USING ERRCODE = '23514';
        END IF;
        SELECT effective.context_frontier_id, effective.member_count
          INTO predecessor_frontier, predecessor_member_count
          FROM turn_start_effective_predecessor_frontier(
                   checked_session,
                   predecessor_frontier
               ) AS effective;
        SELECT CASE
                   WHEN context_frontier_preserves_prefix(
                        checked_session,
                        predecessor_frontier,
                        checked_starting_frontier
                   ) THEN 0
                   ELSE 1
               END
          INTO prefix_mismatch_count;
        IF starting_member_count
               IS DISTINCT FROM predecessor_member_count + turn_lifecycle_origin_member_span(checked_turn_id, checked_session)
               + turn_start_model_identity_entry_count(
                    checked_turn_id,
                    checked_starting_frontier
               )
           OR prefix_mismatch_count <> 0
        THEN
            RAISE EXCEPTION
                'terminal turn % starting frontier does not extend its predecessor',
                checked_turn_id
                USING ERRCODE = '23514';
        END IF;
    ELSE
        RAISE EXCEPTION
            'terminal turn % has unsupported lineage',
            checked_turn_id
            USING ERRCODE = '23514';
    END IF;
END;
$$;


--
-- Name: assert_turn_attempt_final_state(uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION assert_turn_attempt_final_state(checked_attempt_id uuid) RETURNS void
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF EXISTS (
        SELECT 1
          FROM credential_pool_availability_successor AS successor
          JOIN turn_attempt AS successor_attempt
            ON successor_attempt.turn_attempt_id =
               successor.successor_turn_attempt_id
          JOIN model_call AS predecessor
            ON predecessor.model_call_id = successor.predecessor_model_call_id
         WHERE successor.successor_turn_attempt_id = checked_attempt_id
           AND successor_attempt.turn_id = predecessor.turn_id
           AND successor_attempt.session_id = predecessor.session_id
           AND successor_attempt.continued_from_attempt_id =
               predecessor.turn_attempt_id
           AND predecessor.state_kind = 'terminal'
           AND predecessor.terminal_disposition_kind = 'known_failed'
    ) THEN
        RETURN;
    END IF;

    PERFORM assert_turn_attempt_final_state_before_credential_pools(
        checked_attempt_id
    );
END;
$$;


--
-- Name: assert_turn_attempt_final_state_before_credential_pools(uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION assert_turn_attempt_final_state_before_credential_pools(checked_turn_attempt_id uuid) RETURNS void
    LANGUAGE plpgsql
    AS $$
DECLARE
    attempt_record turn_attempt%ROWTYPE;
BEGIN
    SELECT *
      INTO attempt_record
      FROM turn_attempt
     WHERE turn_attempt_id = checked_turn_attempt_id;
    IF NOT FOUND OR attempt_record.continued_from_attempt_id IS NULL THEN
        RETURN;
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM turn_attempt AS predecessor
          JOIN model_call AS call
            ON call.turn_attempt_id = predecessor.turn_attempt_id
           AND call.turn_id = predecessor.turn_id
           AND call.session_id = predecessor.session_id
          JOIN tool_round AS round
            ON round.producing_model_call_id = call.model_call_id
           AND round.turn_id = call.turn_id
           AND round.session_id = call.session_id
         WHERE predecessor.turn_attempt_id =
               attempt_record.continued_from_attempt_id
           AND predecessor.turn_id = attempt_record.turn_id
           AND predecessor.session_id = attempt_record.session_id
           AND predecessor.state_kind = 'ended'
           AND predecessor.end_variant = 'without_stop'
           AND predecessor.end_disposition = 'yielded_to_durable_wait'
           AND call.state_kind = 'terminal'
           AND call.terminal_disposition_kind = 'completed'
           AND round.boundary_kind = 'continuing'
    ) AND NOT EXISTS (
        SELECT 1
          FROM turn_attempt AS predecessor
          JOIN tool_attempt AS child_wait_attempt
            ON child_wait_attempt.issuing_turn_attempt_id =
               predecessor.turn_attempt_id
           AND child_wait_attempt.turn_id = predecessor.turn_id
           AND child_wait_attempt.session_id = predecessor.session_id
          JOIN session_delegation_wait AS waiting
            ON waiting.awaiting_tool_request_id = child_wait_attempt.request_id
           AND waiting.spawning_tool_request_id =
               child_wait_attempt.wait_spawning_request_id
           AND waiting.child_session_id =
               child_wait_attempt.wait_child_session_id
           AND waiting.parent_turn_id = predecessor.turn_id
           AND waiting.parent_session_id = predecessor.session_id
           AND waiting.wait_mode = 'foreground'
          JOIN session_child_result_delivery AS delivery
            ON delivery.awaiting_tool_request_id =
               waiting.awaiting_tool_request_id
           AND delivery.spawning_tool_request_id =
               waiting.spawning_tool_request_id
           AND delivery.parent_session_id = waiting.parent_session_id
           AND delivery.delivery_sequence IS NULL
         WHERE predecessor.turn_attempt_id =
               attempt_record.continued_from_attempt_id
           AND predecessor.turn_id = attempt_record.turn_id
           AND predecessor.session_id = attempt_record.session_id
           AND predecessor.state_kind = 'ended'
           AND predecessor.end_variant = 'without_stop'
           AND predecessor.end_disposition = 'yielded_to_durable_wait'
           AND child_wait_attempt.state_kind = 'terminal'
           AND child_wait_attempt.terminal_disposition_kind = 'awaiting_child'
    ) THEN
        RAISE EXCEPTION
            'turn attempt continuation lacks an exact durable tool yield'
            USING
                ERRCODE = '23514',
                CONSTRAINT = 'turn_attempt_continuation_requires_tool_yield';
    END IF;
END;
$$;


--
-- Name: assert_turn_lifecycle_final_state(uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION assert_turn_lifecycle_final_state(checked_turn_id uuid) RETURNS void
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NOT claim_deferred_final_state_validation('turn_lifecycle', checked_turn_id) THEN
        RETURN;
    END IF;

    IF EXISTS (
        SELECT 1
          FROM tool_round
         WHERE turn_id = checked_turn_id
    ) THEN
        PERFORM assert_tool_loop_turn_final_state(checked_turn_id);
    ELSE
        PERFORM assert_turn_lifecycle_final_state_without_tool_loop(
            checked_turn_id
        );
    END IF;
END;
$$;


--
-- Name: assert_turn_lifecycle_final_state_without_cancellation(uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION assert_turn_lifecycle_final_state_without_cancellation(checked_turn_id uuid) RETURNS void
    LANGUAGE plpgsql
    AS $$
DECLARE
    steering_terminal boolean;
BEGIN
    SELECT EXISTS (
        SELECT 1
          FROM turn_lifecycle AS lifecycle
          JOIN model_call AS call
            ON call.model_call_id = lifecycle.terminal_model_call_id
           AND call.turn_id = lifecycle.turn_id
           AND call.session_id = lifecycle.session_id
         WHERE lifecycle.turn_id = checked_turn_id
           AND lifecycle.state_kind = 'terminal'
           AND call.context_frontier_id <> lifecycle.starting_frontier_id
    )
      INTO steering_terminal;

    IF steering_terminal THEN
        PERFORM assert_steering_turn_terminal_final_state(checked_turn_id);
    ELSE
        PERFORM assert_turn_lifecycle_final_state_without_steering(checked_turn_id);
    END IF;
END;
$$;


--
-- Name: assert_turn_lifecycle_final_state_without_reconciliation(uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION assert_turn_lifecycle_final_state_without_reconciliation(checked_turn_id uuid) RETURNS void
    LANGUAGE plpgsql
    AS $$
DECLARE
    cancelled_terminal boolean;
BEGIN
    SELECT EXISTS (
        SELECT 1
          FROM turn_lifecycle
         WHERE turn_id = checked_turn_id
           AND state_kind = 'terminal'
           AND terminal_disposition_kind = 'cancelled'
    )
      INTO cancelled_terminal;

    IF cancelled_terminal THEN
        PERFORM assert_cancelled_turn_final_state(checked_turn_id);
    ELSE
        PERFORM assert_turn_lifecycle_final_state_without_cancellation(
            checked_turn_id
        );
    END IF;
END;
$$;


--
-- Name: assert_turn_lifecycle_final_state_without_steering(uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION assert_turn_lifecycle_final_state_without_steering(checked_turn_id uuid) RETURNS void
    LANGUAGE plpgsql
    AS $$
DECLARE
    checked_session_id uuid;
    checked_origin_input_id uuid;
    checked_position numeric(20, 0);
    checked_attempt_history_present boolean;
    checked_state text;
    checked_lineage text;
    checked_predecessor uuid;
    checked_starting_frontier uuid;
    checked_terminal_frontier uuid;
    checked_active_phase text;
    checked_current_attempt uuid;
    checked_recovery_call uuid;
    checked_terminal_attempt uuid;
    checked_terminal_call uuid;
    checked_terminal_disposition text;
    attempt_count bigint;
    live_attempt_count bigint;
    exact_attempt_count bigint;
    contradictory_failed_attempt_count bigint;
    origin_entry_count bigint;
    origin_entry_id uuid;
    failure_entry_count bigint;
    failure_entry_id uuid;
    completion_entry_count bigint;
    completion_entry_id uuid;
    assistant_entry_count bigint;
    assistant_member_count bigint;
    origin_member_count bigint;
    origin_member_position numeric(20, 0);
    last_member_position numeric(20, 0);
    failure_member_count bigint;
    starting_member_count numeric(20, 0);
    terminal_member_count numeric(20, 0);
    predecessor_terminal_frontier uuid;
    predecessor_terminal_member_count numeric(20, 0);
    prefix_mismatch_count bigint;
    predecessor_state text;
    predecessor_position numeric(20, 0);
    expected_predecessor_position numeric(20, 0);
BEGIN
    SELECT
        session_id,
        origin_accepted_input_id,
        acceptance_position,
        attempt_history_present,
        state_kind,
        start_lineage_kind,
        immediate_predecessor_turn_id,
        starting_frontier_id,
        terminal_frontier_id,
        active_phase_kind,
        current_attempt_id,
        recovery_model_call_id,
        terminal_attempt_id,
        terminal_model_call_id,
        terminal_disposition_kind
      INTO
        checked_session_id,
        checked_origin_input_id,
        checked_position,
        checked_attempt_history_present,
        checked_state,
        checked_lineage,
        checked_predecessor,
        checked_starting_frontier,
        checked_terminal_frontier,
        checked_active_phase,
        checked_current_attempt,
        checked_recovery_call,
        checked_terminal_attempt,
        checked_terminal_call,
        checked_terminal_disposition
      FROM turn_lifecycle
     WHERE turn_id = checked_turn_id;

    IF NOT FOUND THEN
        RETURN;
    END IF;

    SELECT
        count(*),
        count(*) FILTER (WHERE state_kind <> 'ended'),
        count(*) FILTER (
            WHERE turn_attempt_id = COALESCE(
                checked_current_attempt,
                checked_terminal_attempt
            )
        ),
        count(*) FILTER (
            WHERE state_kind <> 'ended'
               OR end_disposition NOT IN ('known_failure', 'lost')
        )
      INTO
        attempt_count,
        live_attempt_count,
        exact_attempt_count,
        contradictory_failed_attempt_count
      FROM turn_attempt
     WHERE turn_id = checked_turn_id
       AND session_id = checked_session_id;

    IF checked_attempt_history_present IS DISTINCT FROM (attempt_count > 0) THEN
        RAISE EXCEPTION 'turn % attempt marker disagrees with durable attempts', checked_turn_id
            USING ERRCODE = '23514';
    END IF;

    SELECT count(*)
      INTO origin_entry_count
      FROM turn_lifecycle_origin_semantic_entry(
            checked_turn_id,
            checked_session_id,
            checked_origin_input_id
      );

    SELECT semantic_entry_id
      INTO origin_entry_id
      FROM turn_lifecycle_origin_semantic_entry(
            checked_turn_id,
            checked_session_id,
            checked_origin_input_id
      );

    SELECT count(*)
      INTO failure_entry_count
      FROM semantic_transcript_entry
     WHERE source_session_id = checked_session_id
       AND payload_kind = 'turn_failed'
       AND failed_turn_id = checked_turn_id;

    SELECT semantic_entry_id
      INTO failure_entry_id
      FROM semantic_transcript_entry
     WHERE source_session_id = checked_session_id
       AND payload_kind = 'turn_failed'
       AND failed_turn_id = checked_turn_id;

    SELECT count(*)
      INTO completion_entry_count
      FROM semantic_transcript_entry
     WHERE source_session_id = checked_session_id
       AND payload_kind = 'turn_completed'
       AND completed_turn_id = checked_turn_id;

    SELECT semantic_entry_id
      INTO completion_entry_id
      FROM semantic_transcript_entry
     WHERE source_session_id = checked_session_id
       AND payload_kind = 'turn_completed'
       AND completed_turn_id = checked_turn_id;

    IF checked_state = 'queued' THEN
        IF attempt_count <> 0
           OR origin_entry_count <> 0
           OR failure_entry_count <> 0
           OR completion_entry_count <> 0
        THEN
            RAISE EXCEPTION 'queued turn % carries started or terminal facts', checked_turn_id
                USING ERRCODE = '23514';
        END IF;
        RETURN;
    END IF;

    IF origin_entry_count <> 1 THEN
        RAISE EXCEPTION 'started turn % requires its exact origin entry', checked_turn_id
            USING ERRCODE = '23503';
    END IF;

    SELECT member_count
      INTO starting_member_count
      FROM context_frontier
     WHERE owning_session_id = checked_session_id
       AND context_frontier_id = checked_starting_frontier;

    SELECT max(member_position)
      INTO last_member_position
      FROM context_frontier_member
     WHERE owning_session_id = checked_session_id
       AND context_frontier_id = checked_starting_frontier;

    SELECT count(*), max(member_position)
      INTO origin_member_count, origin_member_position
      FROM context_frontier_member
     WHERE owning_session_id = checked_session_id
       AND context_frontier_id = checked_starting_frontier
       AND source_session_id = checked_session_id
       AND semantic_entry_id = origin_entry_id;

    IF origin_member_count <> 1
       OR origin_member_position IS DISTINCT FROM last_member_position
       OR NOT turn_start_model_identity_boundary_is_valid(
            checked_turn_id,
            checked_starting_frontier
       )
    THEN
        RAISE EXCEPTION 'turn % starting frontier does not end in its origin', checked_turn_id
            USING ERRCODE = '23503';
    END IF;

    IF checked_lineage = 'first_in_session' THEN
        IF NOT first_native_starting_frontier_matches_seed(
            checked_session_id,
            checked_starting_frontier
        )
           OR EXISTS (
            SELECT 1
              FROM turn_lifecycle AS earlier
             WHERE earlier.session_id = checked_session_id
               AND earlier.turn_id <> checked_turn_id
               AND earlier.acceptance_position < checked_position
               AND goal_turn_is_runtime_relevant(
                    earlier.session_id,
                    earlier.turn_id
               )
        ) THEN
            RAISE EXCEPTION 'turn % has invalid first lineage', checked_turn_id
                USING ERRCODE = '23514';
        END IF;
    ELSE
        SELECT state_kind, acceptance_position, turn_lifecycle_effective_terminal_frontier(session_id, turn_id)
          INTO predecessor_state, predecessor_position, predecessor_terminal_frontier
          FROM turn_lifecycle
         WHERE turn_id = checked_predecessor
           AND session_id = checked_session_id;

        SELECT acceptance_position
          INTO expected_predecessor_position
          FROM turn_lifecycle
         WHERE session_id = checked_session_id
           AND turn_id = accepted_input_turn_queue_predecessor(
                checked_session_id,
                checked_turn_id
           );

        IF (predecessor_state IS DISTINCT FROM 'terminal'
           AND predecessor_terminal_frontier IS NULL)
           OR predecessor_position IS DISTINCT FROM expected_predecessor_position
        THEN
            RAISE EXCEPTION 'turn % does not follow its immediate terminal predecessor', checked_turn_id
                USING ERRCODE = '23514';
        END IF;

        SELECT effective.context_frontier_id, effective.member_count
          INTO predecessor_terminal_frontier,
               predecessor_terminal_member_count
          FROM turn_start_effective_predecessor_frontier(
                   checked_session_id,
                   predecessor_terminal_frontier
               ) AS effective;

        SELECT CASE
                   WHEN context_frontier_preserves_prefix(
                        checked_session_id,
                        predecessor_terminal_frontier,
                        checked_starting_frontier
                   ) THEN 0
                   ELSE 1
               END
          INTO prefix_mismatch_count;

        IF starting_member_count IS DISTINCT FROM predecessor_terminal_member_count + turn_lifecycle_origin_member_span(checked_turn_id, checked_session_id)
               + turn_start_model_identity_entry_count(
                    checked_turn_id,
                    checked_starting_frontier
               )
           OR prefix_mismatch_count <> 0
        THEN
            RAISE EXCEPTION 'turn % starting frontier is not predecessor prefix plus origin', checked_turn_id
                USING ERRCODE = '23514';
        END IF;
    END IF;

    IF checked_state = 'active' THEN
        IF failure_entry_count <> 0 OR completion_entry_count <> 0 THEN
            RAISE EXCEPTION 'active turn % carries a terminal semantic marker', checked_turn_id
                USING ERRCODE = '23514';
        END IF;

        IF checked_active_phase = 'running' THEN
            IF live_attempt_count <> 1 OR exact_attempt_count <> 1 THEN
                RAISE EXCEPTION 'running turn % requires its exact live attempt', checked_turn_id
                    USING ERRCODE = '23514';
            END IF;
        ELSIF checked_active_phase = 'awaiting_child' THEN
            IF live_attempt_count <> 0 OR exact_attempt_count <> 0 THEN
                RAISE EXCEPTION 'child-wait turn % retains a live current attempt', checked_turn_id
                    USING ERRCODE = '23514';
            END IF;
        ELSIF checked_active_phase = 'awaiting_runner_recovery' THEN
            IF live_attempt_count <> 0 OR exact_attempt_count <> 0 THEN
                RAISE EXCEPTION
                    'runner recovery turn % retains a current attempt',
                    checked_turn_id
                    USING ERRCODE = '23514';
            END IF;
        ELSE
            IF live_attempt_count <> 0
               OR exact_attempt_count <> 1
               OR NOT EXISTS (
                    SELECT 1
                      FROM turn_attempt
                     WHERE turn_attempt_id = checked_current_attempt
                       AND turn_id = checked_turn_id
                       AND session_id = checked_session_id
                       AND state_kind = 'ended'
                       AND end_disposition IN ('ambiguous', 'lost')
               )
               OR NOT EXISTS (
                    SELECT 1
                      FROM model_call
                     WHERE model_call_id = checked_recovery_call
                       AND turn_attempt_id = checked_current_attempt
                       AND turn_id = checked_turn_id
                       AND session_id = checked_session_id
                       AND state_kind = 'terminal'
                       AND terminal_disposition_kind = 'ambiguous'
               )
            THEN
                RAISE EXCEPTION 'turn % has an incomplete model-call recovery wait', checked_turn_id
                    USING ERRCODE = '23514';
            END IF;
        END IF;
        RETURN;
    END IF;

    IF live_attempt_count <> 0 THEN
        RAISE EXCEPTION 'terminal turn % retains a live attempt', checked_turn_id
            USING ERRCODE = '23514';
    END IF;

    SELECT member_count
      INTO terminal_member_count
      FROM context_frontier
     WHERE owning_session_id = checked_session_id
       AND context_frontier_id = checked_terminal_frontier;

    SELECT CASE
               WHEN context_frontier_preserves_prefix(
                    checked_session_id,
                    checked_starting_frontier,
                    checked_terminal_frontier
               ) THEN 0
               ELSE 1
           END
      INTO prefix_mismatch_count;

    IF checked_terminal_disposition = 'failed' THEN
        IF contradictory_failed_attempt_count <> 0 THEN
            RAISE EXCEPTION
                'failed terminal turn % permits only known_failure or lost ended attempts',
                checked_turn_id
                USING ERRCODE = '23514';
        END IF;

        IF failure_entry_count <> 1
           OR completion_entry_count <> 0
           OR EXISTS (
                SELECT 1
                  FROM model_call
                 WHERE turn_id = checked_turn_id
                   AND session_id = checked_session_id
                   AND (
                        state_kind <> 'terminal'
                        OR terminal_disposition_kind NOT IN (
                            'known_failed',
                            'cancelled'
                        )
                   )
           )
        THEN
            RAISE EXCEPTION 'failed turn % has contradictory terminal facts', checked_turn_id
                USING ERRCODE = '23514';
        END IF;

        SELECT count(*)
          INTO failure_member_count
          FROM context_frontier_member
         WHERE owning_session_id = checked_session_id
           AND context_frontier_id = checked_terminal_frontier
           AND source_session_id = checked_session_id
           AND semantic_entry_id = failure_entry_id;

        IF terminal_member_count IS DISTINCT FROM starting_member_count + 1
           OR prefix_mismatch_count <> 0
           OR failure_member_count <> 1
           OR NOT EXISTS (
                SELECT 1
                  FROM context_frontier_member
                 WHERE owning_session_id = checked_session_id
                   AND context_frontier_id = checked_terminal_frontier
                   AND member_position = terminal_member_count
                   AND source_session_id = checked_session_id
                   AND semantic_entry_id = failure_entry_id
           )
        THEN
            RAISE EXCEPTION 'failed turn % terminal frontier is not prefix plus failure', checked_turn_id
                USING ERRCODE = '23514';
        END IF;
    ELSIF checked_terminal_disposition = 'refused' THEN
        IF failure_entry_count <> 0
           OR completion_entry_count <> 0
           OR checked_terminal_frontier = checked_starting_frontier
           OR terminal_member_count IS DISTINCT FROM starting_member_count
           OR prefix_mismatch_count <> 0
           OR NOT EXISTS (
                SELECT 1
                  FROM turn_attempt
                 WHERE turn_attempt_id = checked_terminal_attempt
                   AND end_disposition IN ('turn_refused', 'lost')
           )
           OR NOT EXISTS (
                SELECT 1
                  FROM model_call
                 WHERE model_call_id = checked_terminal_call
                   AND turn_attempt_id = checked_terminal_attempt
                   AND terminal_disposition_kind = 'refused'
           )
        THEN
            RAISE EXCEPTION 'refused turn % lacks its exact equal-content boundary', checked_turn_id
                USING ERRCODE = '23514';
        END IF;
    ELSE
        SELECT count(*)
          INTO assistant_entry_count
          FROM semantic_transcript_entry
         WHERE source_session_id = checked_session_id
           AND payload_kind = 'assistant_text'
           AND producing_model_call_id = checked_terminal_call;

        SELECT count(*)
          INTO assistant_member_count
          FROM context_frontier_member AS member
          JOIN semantic_transcript_entry AS entry
            ON entry.source_session_id = member.source_session_id
           AND entry.semantic_entry_id = member.semantic_entry_id
         WHERE member.owning_session_id = checked_session_id
           AND member.context_frontier_id = checked_terminal_frontier
           AND member.member_position > starting_member_count
           AND member.member_position < terminal_member_count
           AND entry.payload_kind = 'assistant_text'
           AND entry.producing_model_call_id = checked_terminal_call;

        IF failure_entry_count <> 0
           OR completion_entry_count <> 1
           OR terminal_member_count
                IS DISTINCT FROM starting_member_count + assistant_entry_count + 1
           OR prefix_mismatch_count <> 0
           OR assistant_member_count <> assistant_entry_count
           OR NOT EXISTS (
                SELECT 1
                  FROM context_frontier_member
                 WHERE owning_session_id = checked_session_id
                   AND context_frontier_id = checked_terminal_frontier
                   AND member_position = terminal_member_count
                   AND source_session_id = checked_session_id
                   AND semantic_entry_id = completion_entry_id
           )
           OR NOT EXISTS (
                SELECT 1
                  FROM turn_attempt
                 WHERE turn_attempt_id = checked_terminal_attempt
                   AND end_disposition IN ('turn_completed', 'lost')
           )
           OR NOT EXISTS (
                SELECT 1
                  FROM model_call
                 WHERE model_call_id = checked_terminal_call
                   AND turn_attempt_id = checked_terminal_attempt
                   AND terminal_disposition_kind = 'completed'
           )
        THEN
            RAISE EXCEPTION 'completed turn % lacks its atomic ordered response boundary', checked_turn_id
                USING ERRCODE = '23514';
        END IF;
    END IF;
END;
$$;


--
-- Name: assert_turn_lifecycle_final_state_without_tool_loop(uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION assert_turn_lifecycle_final_state_without_tool_loop(checked_turn_id uuid) RETURNS void
    LANGUAGE plpgsql
    AS $$
DECLARE
    reconciliation_terminal boolean;
BEGIN
    SELECT EXISTS (
        SELECT 1
          FROM turn_lifecycle
         WHERE turn_id = checked_turn_id
           AND state_kind = 'terminal'
           AND terminal_disposition_kind = 'reconciliation_required'
    )
      INTO reconciliation_terminal;

    IF reconciliation_terminal THEN
        PERFORM assert_reconciliation_required_turn_final_state(
            checked_turn_id
        );
    ELSE
        PERFORM assert_turn_lifecycle_final_state_without_reconciliation(
            checked_turn_id
        );
    END IF;
END;
$$;


--
-- Name: assert_turn_runner_recovery_complete(uuid, uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION assert_turn_runner_recovery_complete(checked_session_id uuid, checked_turn_id uuid) RETURNS void
    LANGUAGE plpgsql
    AS $$
DECLARE
    lifecycle turn_lifecycle%ROWTYPE;
    placement runner_session_placement_record%ROWTYPE;
    yielded_attempt_count bigint;
BEGIN
    SELECT * INTO lifecycle
      FROM turn_lifecycle
     WHERE session_id = checked_session_id
       AND turn_id = checked_turn_id;
    IF NOT FOUND OR lifecycle.active_phase_kind IS DISTINCT FROM
        'awaiting_runner_recovery'
    THEN
        RETURN;
    END IF;

    -- Both the lifecycle-side and placement-side deferred checks rendezvous on
    -- the scheduler row.  Ordinary lifecycle checks return before adding a
    -- reverse lifecycle-to-scheduler lock edge.  A recovery waiter that lost
    -- the race then evaluates the relationship from a fresh READ COMMITTED
    -- statement snapshot.
    PERFORM 1
      FROM session_scheduler
     WHERE session_id = checked_session_id
     FOR UPDATE;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'runner recovery wait lacks its session scheduler'
            USING ERRCODE = '23514';
    END IF;

    SELECT * INTO lifecycle
      FROM turn_lifecycle
     WHERE session_id = checked_session_id
       AND turn_id = checked_turn_id;
    IF NOT FOUND OR lifecycle.active_phase_kind IS DISTINCT FROM
        'awaiting_runner_recovery'
    THEN
        RETURN;
    END IF;

    SELECT record.* INTO placement
      FROM runner_current_session_placement AS current_placement
      JOIN runner_session_placement_record AS record
        ON record.session_id = current_placement.session_id
       AND record.event_ordinal = current_placement.event_ordinal
     WHERE current_placement.session_id = checked_session_id;
    IF NOT FOUND
       OR placement.state_kind NOT IN ('runner_lost', 'runner_lost_before_pin')
       OR placement.lost_runner_id IS DISTINCT FROM
            lifecycle.runner_recovery_runner_id
       OR placement.placement_revision IS DISTINCT FROM
            lifecycle.runner_recovery_placement_revision
       OR placement.interrupted_tool_attempt_id IS DISTINCT FROM
            lifecycle.runner_recovery_tool_attempt_id
    THEN
        RAISE EXCEPTION
            'runner recovery wait lacks its exact current lost placement'
            USING ERRCODE = '23514';
    END IF;
    SELECT count(*) INTO yielded_attempt_count
      FROM turn_attempt AS yielded_attempt
     WHERE yielded_attempt.turn_id = lifecycle.turn_id
       AND yielded_attempt.session_id = lifecycle.session_id
       AND yielded_attempt.state_kind = 'ended'
       AND yielded_attempt.end_variant = 'without_stop'
       AND yielded_attempt.end_disposition = 'yielded_to_durable_wait'
       AND yielded_attempt.interrupt_command_id IS NULL
       AND yielded_attempt.interrupt_predecessor_turn_id IS NULL
       AND NOT EXISTS (
            SELECT 1
              FROM turn_attempt AS continuation
             WHERE continuation.continued_from_attempt_id =
                    yielded_attempt.turn_attempt_id
       );
    IF yielded_attempt_count <> 1 THEN
        RAISE EXCEPTION
            'runner recovery wait lacks its exact yielded turn boundary'
            USING ERRCODE = '23514';
    END IF;
    IF lifecycle.runner_recovery_tool_attempt_id IS NULL
       AND lifecycle.active_tool_round_call_id IS NULL
       AND EXISTS (
            SELECT 1
              FROM turn_attempt AS yielded_attempt
              JOIN model_call AS producing_call
                ON producing_call.turn_attempt_id =
                    yielded_attempt.turn_attempt_id
               AND producing_call.turn_id = yielded_attempt.turn_id
               AND producing_call.session_id = yielded_attempt.session_id
              JOIN tool_round AS round
                ON round.producing_model_call_id =
                    producing_call.model_call_id
               AND round.turn_id = producing_call.turn_id
               AND round.session_id = producing_call.session_id
             WHERE yielded_attempt.turn_id = lifecycle.turn_id
               AND yielded_attempt.session_id = lifecycle.session_id
               AND yielded_attempt.state_kind = 'ended'
               AND yielded_attempt.end_variant = 'without_stop'
               AND yielded_attempt.end_disposition =
                    'yielded_to_durable_wait'
               AND yielded_attempt.interrupt_command_id IS NULL
               AND yielded_attempt.interrupt_predecessor_turn_id IS NULL
               AND NOT EXISTS (
                    SELECT 1
                      FROM turn_attempt AS continuation
                     WHERE continuation.continued_from_attempt_id =
                            yielded_attempt.turn_attempt_id
               )
               AND round.boundary_kind = 'continuing'
       )
    THEN
        RAISE EXCEPTION
            'runner recovery wait cannot hide its yielded tool round'
            USING ERRCODE = '23514';
    END IF;
    IF lifecycle.runner_recovery_tool_attempt_id IS NULL
       AND lifecycle.active_tool_round_call_id IS NOT NULL
       AND (
            NOT EXISTS (
                SELECT 1
                  FROM model_call AS active_call
                  JOIN tool_round AS active_round
                    ON active_round.producing_model_call_id =
                        active_call.model_call_id
                   AND active_round.turn_id = active_call.turn_id
                   AND active_round.session_id = active_call.session_id
                  JOIN turn_attempt AS yielded_attempt
                    ON yielded_attempt.turn_attempt_id =
                        active_call.turn_attempt_id
                   AND yielded_attempt.turn_id = active_call.turn_id
                   AND yielded_attempt.session_id = active_call.session_id
                 WHERE active_call.model_call_id =
                        lifecycle.active_tool_round_call_id
                   AND active_call.turn_id = checked_turn_id
                   AND active_call.session_id = checked_session_id
                   AND active_round.boundary_kind = 'continuing'
                   AND yielded_attempt.state_kind = 'ended'
                   AND yielded_attempt.end_variant = 'without_stop'
                   AND yielded_attempt.end_disposition =
                        'yielded_to_durable_wait'
                   AND yielded_attempt.interrupt_command_id IS NULL
                   AND yielded_attempt.interrupt_predecessor_turn_id IS NULL
                   AND NOT EXISTS (
                        SELECT 1
                          FROM turn_attempt AS continuation
                         WHERE continuation.continued_from_attempt_id =
                                yielded_attempt.turn_attempt_id
                   )
            )
            OR EXISTS (
                SELECT 1
                  FROM tool_request AS request
                  JOIN runner_current_tool_attempt AS attempt
                    ON attempt.request_id = request.request_id
                   AND attempt.turn_id = request.turn_id
                   AND attempt.session_id = request.session_id
                 WHERE request.producing_model_call_id =
                        lifecycle.active_tool_round_call_id
                   AND request.turn_id = checked_turn_id
                   AND request.session_id = checked_session_id
                   AND (
                        attempt.state_kind IN ('prepared', 'in_flight')
                        OR (
                            attempt.state_kind = 'terminal'
                            AND attempt.terminal_disposition_kind = 'ambiguous'
                        )
                   )
            )
       )
    THEN
        RAISE EXCEPTION
            'runner recovery tool round lacks its exact yielded turn boundary'
            USING ERRCODE = '23514';
    END IF;
    IF lifecycle.runner_recovery_tool_attempt_id IS NOT NULL
       AND (
        NOT EXISTS (
            SELECT 1
              FROM tool_attempt AS attempt
              JOIN tool_request AS request
               ON request.request_id = attempt.request_id
               AND request.turn_id = attempt.turn_id
               AND request.session_id = attempt.session_id
              JOIN tool_round AS active_round
                ON active_round.producing_model_call_id =
                    request.producing_model_call_id
               AND active_round.turn_id = request.turn_id
               AND active_round.session_id = request.session_id
              JOIN turn_attempt AS yielded_attempt
                ON yielded_attempt.turn_attempt_id =
                    attempt.issuing_turn_attempt_id
               AND yielded_attempt.turn_id = attempt.turn_id
               AND yielded_attempt.session_id = attempt.session_id
              JOIN runner_physical_attempt_lease_binding AS binding
                ON binding.attempt_id = attempt.attempt_id
              JOIN runner_lease_generation AS lease
                ON lease.lease_id = binding.lease_id
               AND lease.attempt_id = attempt.attempt_id
               AND lease.session_id = attempt.session_id
              JOIN runner_current_lease_event AS current_lease
                ON current_lease.lease_id = lease.lease_id
               AND current_lease.generation = lease.generation
              JOIN runner_lease_event AS lease_event
                ON lease_event.lease_id = current_lease.lease_id
               AND lease_event.generation = current_lease.generation
               AND lease_event.event_ordinal = current_lease.event_ordinal
              JOIN runner_session_placement_record AS leased_placement
                ON leased_placement.session_id = lease.session_id
               AND leased_placement.event_ordinal =
                    lease.placement_event_ordinal
             WHERE attempt.attempt_id =
                    lifecycle.runner_recovery_tool_attempt_id
               AND attempt.turn_id = checked_turn_id
               AND attempt.session_id = checked_session_id
               AND (
                    (
                        attempt.state_kind = 'in_flight'
                        AND (
                            lease_event.state_kind = 'lost_unclaimed'
                            OR (
                                lease_event.state_kind IN (
                                    'lost_execution_possible',
                                    'lost_claimed'
                                )
                                AND lease.effect_class IN (
                                    'pure', 'idempotent'
                                )
                            )
                        )
                    )
                    OR (
                        attempt.state_kind = 'terminal'
                        AND attempt.terminal_disposition_kind = 'ambiguous'
                        AND lease_event.state_kind IN (
                            'lost_execution_possible',
                            'lost_claimed'
                        )
                        AND lease.effect_class = 'side_effecting'
                    )
               )
               AND request.producing_model_call_id =
                    lifecycle.active_tool_round_call_id
               AND active_round.boundary_kind = 'continuing'
               AND yielded_attempt.state_kind = 'ended'
               AND yielded_attempt.end_variant = 'without_stop'
               AND yielded_attempt.end_disposition =
                    'yielded_to_durable_wait'
               AND yielded_attempt.interrupt_command_id IS NULL
               AND yielded_attempt.interrupt_predecessor_turn_id IS NULL
               AND NOT EXISTS (
                    SELECT 1
                      FROM turn_attempt AS continuation
                     WHERE continuation.continued_from_attempt_id =
                            yielded_attempt.turn_attempt_id
               )
               AND lease.runner_id = lifecycle.runner_recovery_runner_id
               AND runner_lease_placement_reaches_loss_revision(
                    lease.session_id,
                    lease.placement_event_ordinal,
                    lifecycle.runner_recovery_placement_revision,
                    lifecycle.runner_recovery_runner_id
               )
               AND leased_placement.state_kind = 'pinned'
               AND leased_placement.pinned_runner_id =
                    lifecycle.runner_recovery_runner_id
        )
        OR EXISTS (
            SELECT 1
              FROM tool_request AS request
              JOIN runner_current_tool_attempt AS attempt
                ON attempt.request_id = request.request_id
               AND attempt.turn_id = request.turn_id
               AND attempt.session_id = request.session_id
             WHERE request.producing_model_call_id =
                    lifecycle.active_tool_round_call_id
               AND request.turn_id = checked_turn_id
               AND request.session_id = checked_session_id
               AND attempt.attempt_id <>
                    lifecycle.runner_recovery_tool_attempt_id
               AND (
                    attempt.state_kind IN ('prepared', 'in_flight')
                    OR (
                        attempt.state_kind = 'terminal'
                        AND attempt.terminal_disposition_kind = 'ambiguous'
                    )
               )
        )
       )
    THEN
        RAISE EXCEPTION
            'runner recovery tool attempt lacks its exact active tool round'
            USING ERRCODE = '23514';
    END IF;
END;
$$;


--
-- Name: guard_turn_runner_recovery_interrupt_effect(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION guard_turn_runner_recovery_interrupt_effect() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM submit_input_command AS command
          JOIN turn_lifecycle AS lifecycle
            ON lifecycle.session_id = command.session_id
           AND lifecycle.turn_id = command.expected_active_turn_id
          JOIN runner_current_session_placement AS head
            ON head.session_id = lifecycle.session_id
          JOIN runner_session_placement_record AS placement
            ON placement.session_id = head.session_id
           AND placement.event_ordinal = head.event_ordinal
         WHERE command.command_id = NEW.command_id
           AND command.delivery_kind = 'interrupt'
           AND command.result_kind = 'applied'
           AND command.session_id = NEW.session_id
           AND command.expected_active_turn_id = NEW.turn_id
           AND lifecycle.state_kind = 'active'
           AND lifecycle.active_phase_kind = 'awaiting_runner_recovery'
           AND lifecycle.runner_recovery_runner_id = NEW.runner_id
           AND lifecycle.runner_recovery_placement_revision =
                NEW.placement_revision
           AND lifecycle.runner_recovery_tool_attempt_id IS NOT DISTINCT FROM
                NEW.interrupted_tool_attempt_id
           AND (
                (
                    lifecycle.active_tool_round_call_id IS NULL
                    AND NEW.source_frontier_id = lifecycle.starting_frontier_id
                )
                OR EXISTS (
                    SELECT 1
                      FROM tool_round AS round
                     WHERE round.producing_model_call_id =
                            lifecycle.active_tool_round_call_id
                       AND round.turn_id = lifecycle.turn_id
                       AND round.session_id = lifecycle.session_id
                       AND round.boundary_kind = 'continuing'
                       AND round.boundary_frontier_id = NEW.source_frontier_id
                )
           )
           AND EXISTS (
                SELECT 1
                  FROM turn_attempt AS yielded_attempt
                 WHERE yielded_attempt.turn_attempt_id =
                        NEW.yielded_turn_attempt_id
                   AND yielded_attempt.turn_id = lifecycle.turn_id
                   AND yielded_attempt.session_id = lifecycle.session_id
                   AND yielded_attempt.state_kind = 'ended'
                   AND yielded_attempt.end_variant = 'without_stop'
                   AND yielded_attempt.end_disposition =
                        'yielded_to_durable_wait'
                   AND yielded_attempt.interrupt_command_id IS NULL
                   AND yielded_attempt.interrupt_predecessor_turn_id IS NULL
                   AND NOT EXISTS (
                        SELECT 1
                          FROM turn_attempt AS continuation
                         WHERE continuation.continued_from_attempt_id =
                                yielded_attempt.turn_attempt_id
                   )
           )
           AND head.event_ordinal = NEW.placement_event_ordinal
           AND placement.state_kind IN ('runner_lost', 'runner_lost_before_pin')
           AND placement.lost_runner_id = NEW.runner_id
           AND placement.placement_revision = NEW.placement_revision
           AND placement.interrupted_tool_attempt_id IS NOT DISTINCT FROM
                NEW.interrupted_tool_attempt_id
    ) THEN
        RAISE EXCEPTION
            'runner recovery interrupt effect lacks exact active loss authority'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: recheck_session_turn_runner_recovery(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION recheck_session_turn_runner_recovery() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    checked_session_id uuid;
    checked_turn_id uuid;
BEGIN
    IF TG_TABLE_NAME IN ('runner_lease_event', 'runner_current_lease_event') THEN
        SELECT session_id INTO checked_session_id
          FROM runner_lease_generation
         WHERE lease_id = COALESCE(NEW.lease_id, OLD.lease_id)
           AND generation = COALESCE(NEW.generation, OLD.generation);
        IF NOT FOUND THEN
            RAISE EXCEPTION 'runner recovery lease recheck lacks its generation'
                USING ERRCODE = '23514';
        END IF;
    ELSIF TG_OP = 'DELETE' THEN
        checked_session_id := OLD.session_id;
    ELSE
        checked_session_id := NEW.session_id;
    END IF;

    PERFORM 1
      FROM session_scheduler
     WHERE session_id = checked_session_id
     FOR UPDATE;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'runner recovery recheck lacks its session scheduler'
            USING ERRCODE = '23514';
    END IF;

    FOR checked_turn_id IN
        SELECT turn_id
          FROM turn_lifecycle
         WHERE session_id = checked_session_id
           AND active_phase_kind = 'awaiting_runner_recovery'
           AND NOT delegation_runtime_terminal
    LOOP
        PERFORM assert_turn_runner_recovery_complete(
            checked_session_id,
            checked_turn_id
        );
    END LOOP;
    RETURN NULL;
END;
$$;


--
-- Name: reject_accepted_input_descendant_scope_change(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_accepted_input_descendant_scope_change() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF OLD.descendant_scope IS DISTINCT FROM NEW.descendant_scope THEN
        RAISE EXCEPTION 'accepted input descendant scope is immutable'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: reject_content_part_insert_after_parent_transaction(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_content_part_insert_after_parent_transaction() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE parent_creation_xid xid8;
BEGIN
    IF TG_TABLE_NAME = 'submit_input_command_content_part' THEN
        SELECT parent.content_parts_creation_xid
          INTO parent_creation_xid
          FROM submit_input_command AS parent
         WHERE parent.command_id = NEW.command_id;
    ELSE
        SELECT parent.content_parts_creation_xid
          INTO parent_creation_xid
          FROM accepted_input AS parent
         WHERE parent.accepted_input_id = NEW.accepted_input_id;
    END IF;

    IF parent_creation_xid IS DISTINCT FROM pg_current_xact_id() THEN
        RAISE EXCEPTION 'content parts are immutable after parent creation'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: reject_invalid_accepted_input_change(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_invalid_accepted_input_change() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'accepted_input is not deletable'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.disposition_kind = 'pending_steering'
       AND NEW.disposition_kind IN (
            'consumed_as_steering',
            'reclassified_as_turn_origin'
       )
       AND OLD.origin_turn_id IS NULL
       AND (
            (
                NEW.disposition_kind = 'consumed_as_steering'
                AND NEW.origin_turn_id IS NULL
                AND OLD.consuming_model_call_id IS NULL
                AND NEW.consuming_model_call_id IS NOT NULL
            )
            OR
            (
                NEW.disposition_kind = 'reclassified_as_turn_origin'
                AND NEW.origin_turn_id IS NOT NULL
                AND OLD.consuming_model_call_id IS NULL
                AND NEW.consuming_model_call_id IS NULL
            )
       )
       AND ROW(
            OLD.accepted_input_id,
            OLD.accepting_command_id,
            OLD.session_id,
            OLD.delivery_kind,
            OLD.expected_active_turn_id,
            OLD.expected_defaults_version,
            OLD.model_override_kind,
            OLD.replacement_model_kind,
            OLD.replacement_direct_model_selection_id,
            OLD.replacement_model_alias_id,
            OLD.acceptance_position
       ) IS NOT DISTINCT FROM ROW(
            NEW.accepted_input_id,
            NEW.accepting_command_id,
            NEW.session_id,
            NEW.delivery_kind,
            NEW.expected_active_turn_id,
            NEW.expected_defaults_version,
            NEW.model_override_kind,
            NEW.replacement_model_kind,
            NEW.replacement_direct_model_selection_id,
            NEW.replacement_model_alias_id,
            NEW.acceptance_position
       )
    THEN
        RETURN NEW;
    END IF;

    RAISE EXCEPTION
        'accepted_input is immutable outside pending-steering disposition'
        USING ERRCODE = '23514';
END;
$$;


--
-- Name: reject_turn_attempt_invalid_change(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_turn_attempt_invalid_change() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    owning_turn_state text;
BEGIN
    IF TG_OP = 'INSERT' THEN
        UPDATE turn_lifecycle
           SET attempt_history_present = true
         WHERE turn_id = NEW.turn_id
           AND session_id = NEW.session_id
           AND state_kind <> 'terminal'
        RETURNING state_kind
          INTO owning_turn_state;

        IF NOT FOUND THEN
            RAISE EXCEPTION 'a terminal turn cannot acquire another attempt'
                USING ERRCODE = '23514';
        END IF;

        IF NEW.state_kind <> 'prepared' THEN
            RAISE EXCEPTION 'turn attempt must be inserted as prepared'
                USING
                    ERRCODE = '23514',
                    CONSTRAINT = 'turn_attempt_inserted_prepared';
        END IF;
        RETURN NEW;
    END IF;

    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'turn_attempt is not deletable'
            USING ERRCODE = '23514';
    END IF;

    IF ROW(
        OLD.turn_attempt_id,
        OLD.turn_id,
        OLD.session_id,
        OLD.continued_from_attempt_id
    ) IS DISTINCT FROM ROW(
        NEW.turn_attempt_id,
        NEW.turn_id,
        NEW.session_id,
        NEW.continued_from_attempt_id
    ) THEN
        RAISE EXCEPTION 'turn attempt identity, ownership, and predecessor are immutable'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.state_kind = 'ended' THEN
        RAISE EXCEPTION 'ended turn attempt is immutable'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.interrupt_command_id IS NOT NULL
       AND ROW(
            OLD.interrupt_command_id,
            OLD.interrupt_predecessor_turn_id
       ) IS DISTINCT FROM ROW(
            NEW.interrupt_command_id,
            NEW.interrupt_predecessor_turn_id
       )
    THEN
        RAISE EXCEPTION 'turn attempt interrupt proof is immutable'
            USING ERRCODE = '23514';
    END IF;

    IF NOT (
        OLD.state_kind = NEW.state_kind
        OR (
            OLD.state_kind = 'prepared'
            AND NEW.state_kind IN ('running', 'ended')
        )
        OR (
            OLD.state_kind = 'running'
            AND NEW.state_kind IN ('stop_requested', 'ended')
        )
        OR (
            OLD.state_kind = 'stop_requested'
            AND NEW.state_kind = 'ended'
        )
    ) THEN
        RAISE EXCEPTION 'turn attempt transition is not monotonic'
            USING ERRCODE = '23514';
    END IF;

    RETURN NEW;
END;
$$;


--
-- Name: reject_turn_delegation_runtime_terminal_reversal(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_turn_delegation_runtime_terminal_reversal() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF OLD.delegation_runtime_terminal AND NOT NEW.delegation_runtime_terminal THEN
        RAISE EXCEPTION 'delegation runtime terminalization is monotonic'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: reject_turn_lifecycle_invalid_change(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_turn_lifecycle_invalid_change() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        IF NEW.state_kind <> 'queued' THEN
            RAISE EXCEPTION 'turn lifecycle must be inserted as queued'
                USING
                    ERRCODE = '23514',
                    CONSTRAINT = 'turn_lifecycle_inserted_queued';
        END IF;
        IF NEW.attempt_history_present THEN
            RAISE EXCEPTION 'turn lifecycle must be inserted without attempt history'
                USING ERRCODE = '23514';
        END IF;
        IF NEW.pinned_provider_model_identity_id IS NOT NULL THEN
            RAISE EXCEPTION 'queued turn lifecycle cannot begin with a provider target pin'
                USING ERRCODE = '23514';
        END IF;
        RETURN NEW;
    END IF;

    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'turn_lifecycle is not deletable'
            USING ERRCODE = '23514';
    END IF;

    IF ROW(
        OLD.turn_id,
        OLD.session_id,
        OLD.origin_accepted_input_id,
        OLD.acceptance_position
    ) IS DISTINCT FROM ROW(
        NEW.turn_id,
        NEW.session_id,
        NEW.origin_accepted_input_id,
        NEW.acceptance_position
    ) THEN
        RAISE EXCEPTION 'turn lifecycle identity, ownership, origin, and order are immutable'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.start_lineage_kind IS NOT NULL
       AND ROW(
            OLD.start_lineage_kind,
            OLD.immediate_predecessor_turn_id,
            OLD.starting_frontier_id
       ) IS DISTINCT FROM ROW(
            NEW.start_lineage_kind,
            NEW.immediate_predecessor_turn_id,
            NEW.starting_frontier_id
       )
    THEN
        RAISE EXCEPTION 'turn start is write-once'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.pinned_provider_model_identity_id IS NOT NULL
       AND NEW.pinned_provider_model_identity_id
           IS DISTINCT FROM OLD.pinned_provider_model_identity_id
    THEN
        RAISE EXCEPTION 'turn-level provider target pin is immutable'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.pinned_provider_model_identity_id IS NULL
       AND NEW.pinned_provider_model_identity_id IS NOT NULL
       AND (
            OLD.state_kind IS DISTINCT FROM 'active'
            OR NEW.state_kind IS DISTINCT FROM 'active'
            OR OLD.active_phase_kind IS DISTINCT FROM 'running'
            OR NEW.active_phase_kind IS DISTINCT FROM 'running'
            OR OLD.current_attempt_id IS NULL
            OR NEW.current_attempt_id IS DISTINCT FROM OLD.current_attempt_id
       )
    THEN
        RAISE EXCEPTION 'provider target can be pinned only for the current running attempt'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.state_kind = 'terminal' THEN
        RAISE EXCEPTION 'terminal turn lifecycle is immutable'
            USING ERRCODE = '23514';
    END IF;
    IF OLD.attempt_history_present AND NOT NEW.attempt_history_present THEN
        RAISE EXCEPTION 'turn attempt history marker is write-once'
            USING ERRCODE = '23514';
    END IF;
    IF NOT (
        OLD.state_kind = NEW.state_kind
        OR (OLD.state_kind = 'queued' AND NEW.state_kind IN ('active', 'terminal'))
        OR (OLD.state_kind = 'active' AND NEW.state_kind = 'terminal')
    ) THEN
        RAISE EXCEPTION 'turn lifecycle transition is not monotonic'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.state_kind = 'active'
       AND OLD.active_phase_kind IN (
            'awaiting_model_call_recovery',
            'awaiting_tool_recovery'
       )
       AND NEW.state_kind = 'active'
    THEN
        RAISE EXCEPTION 'recovery wait cannot reopen without a recovery decision'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.state_kind = 'active'
       AND OLD.active_phase_kind = 'running'
       AND NEW.state_kind = 'active'
       AND NEW.active_phase_kind = 'running'
       AND OLD.current_attempt_id IS DISTINCT FROM NEW.current_attempt_id
       AND NOT EXISTS (
            SELECT 1
              FROM credential_pool_availability_successor AS successor
              JOIN model_call AS predecessor
                ON predecessor.model_call_id = successor.predecessor_model_call_id
             WHERE successor.successor_turn_attempt_id = NEW.current_attempt_id
               AND predecessor.turn_attempt_id = OLD.current_attempt_id
               AND predecessor.turn_id = OLD.turn_id
               AND predecessor.session_id = OLD.session_id
               AND predecessor.state_kind = 'terminal'
               AND predecessor.terminal_disposition_kind = 'known_failed'
       )
       AND (
            NEW.active_tool_round_call_id IS NULL
            OR NOT EXISTS (
                SELECT 1
                  FROM turn_attempt
                 WHERE turn_attempt_id = OLD.current_attempt_id
                   AND turn_id = OLD.turn_id
                   AND session_id = OLD.session_id
                   AND state_kind = 'ended'
                   AND end_disposition = 'yielded_to_durable_wait'
            )
       )
    THEN
        RAISE EXCEPTION 'running turn cannot replace its current attempt'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.state_kind = 'queued'
       AND NEW.state_kind = 'terminal'
       AND NEW.attempt_history_present
    THEN
        RAISE EXCEPTION 'a queued turn must terminalize without attempt history'
            USING
                ERRCODE = '23514',
                CONSTRAINT = 'turn_lifecycle_queued_failure_without_attempt';
    END IF;

    RETURN NEW;
END;
$$;


--
-- Name: reject_turn_lifecycle_origin_kind_change(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_turn_lifecycle_origin_kind_change() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF OLD.origin_kind <> NEW.origin_kind THEN
        RAISE EXCEPTION 'turn lifecycle origin kind is immutable'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: reject_turn_restart_recovery_origin_truncate(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_turn_restart_recovery_origin_truncate() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    RAISE EXCEPTION '% cannot be truncated', TG_TABLE_NAME
        USING ERRCODE = '23514';
END;
$$;


--
-- Name: require_accepted_input_parts(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_accepted_input_parts() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE checked_input uuid;
BEGIN
    checked_input := CASE WHEN TG_TABLE_NAME = 'accepted_input'
        THEN NEW.accepted_input_id
        ELSE COALESCE(NEW.accepted_input_id, OLD.accepted_input_id) END;
    IF NOT accepted_input_parts_are_valid(checked_input)
       OR NOT accepted_input_parts_match_command(checked_input)
    THEN
        RAISE EXCEPTION 'accepted input has invalid ordered content parts'
            USING ERRCODE = '23514',
                CONSTRAINT = 'accepted_input_content_parts_valid';
    END IF;
    RETURN NULL;
END;
$$;


--
-- Name: require_accepted_input_source(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_accepted_input_source() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE goal_sources bigint;
BEGIN
    SELECT count(*) INTO goal_sources FROM goal_turn
     WHERE accepted_input_id = NEW.accepted_input_id;
    IF NEW.accepting_command_id IS NULL AND goal_sources <> 1 THEN
        RAISE EXCEPTION 'accepted input without a command requires exactly one goal source'
            USING ERRCODE = '23514', CONSTRAINT = 'accepted_input_source_closed';
    END IF;
    RETURN NULL;
END;
$$;


--
-- Name: require_accepted_input_steering_final_state(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_accepted_input_steering_final_state() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    PERFORM assert_steering_accepted_input_final_state(
        CASE
            WHEN TG_OP = 'DELETE' THEN OLD.accepted_input_id
            ELSE NEW.accepted_input_id
        END
    );
    RETURN NULL;
END;
$$;


--
-- Name: require_failed_terminal_execution_final_state(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_failed_terminal_execution_final_state() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    checked_turn_id uuid;
BEGIN
    checked_turn_id := CASE
        WHEN TG_OP = 'DELETE' THEN OLD.turn_id
        ELSE NEW.turn_id
    END;
    PERFORM assert_failed_terminal_execution_final_state(checked_turn_id);
    RETURN NULL;
END;
$$;


--
-- Name: require_interrupt_attempt_proof(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_interrupt_attempt_proof() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    PERFORM assert_interrupt_attempt_proof(
        CASE
            WHEN TG_OP = 'DELETE' THEN OLD.turn_attempt_id
            ELSE NEW.turn_attempt_id
        END
    );
    RETURN NULL;
END;
$$;


--
-- Name: require_interrupt_submit_input_effect_correlation(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_interrupt_submit_input_effect_correlation() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    matching_records bigint;
BEGIN
    IF NEW.result_kind = 'applied'
       AND EXISTS (
            SELECT 1
              FROM turn_runner_recovery_interrupt_effect
             WHERE command_id = NEW.command_id
       )
    THEN
        SELECT count(*)
          INTO matching_records
          FROM turn_runner_recovery_interrupt_effect AS effect
          JOIN accepted_input AS accepted
            ON accepted.accepting_command_id = effect.command_id
           AND accepted.session_id = effect.session_id
          JOIN queued_input_origin AS successor
            ON successor.accepted_input_id = accepted.accepted_input_id
           AND successor.turn_id = accepted.origin_turn_id
           AND successor.session_id = accepted.session_id
           AND successor.acceptance_position = accepted.acceptance_position
          JOIN turn_lifecycle AS cancelled
            ON cancelled.session_id = effect.session_id
           AND cancelled.turn_id = effect.turn_id
          JOIN turn_attempt AS yielded_attempt
            ON yielded_attempt.turn_attempt_id = effect.yielded_turn_attempt_id
           AND yielded_attempt.turn_id = effect.turn_id
           AND yielded_attempt.session_id = effect.session_id
         WHERE effect.command_id = NEW.command_id
           AND effect.session_id = NEW.session_id
           AND effect.turn_id = NEW.expected_active_turn_id
           AND accepted.accepted_input_id = NEW.result_accepted_input_id
           AND accepted.session_id = NEW.result_session_id
           AND accepted_input_parts_match_command(accepted.accepted_input_id)
           AND accepted.delivery_kind = 'interrupt'
           AND accepted.expected_active_turn_id = NEW.expected_active_turn_id
           AND accepted.expected_defaults_version = NEW.expected_defaults_version
           AND accepted.model_override_kind = NEW.model_override_kind
           AND accepted.replacement_model_kind
               IS NOT DISTINCT FROM NEW.replacement_model_kind
           AND accepted.replacement_direct_model_selection_id
               IS NOT DISTINCT FROM NEW.replacement_direct_model_selection_id
           AND accepted.replacement_model_alias_id
               IS NOT DISTINCT FROM NEW.replacement_model_alias_id
           AND accepted.disposition_kind = 'origin_of'
           AND accepted.origin_turn_id = NEW.result_turn_id
           AND successor.priority_kind = 'interrupt_immediately_after'
           AND successor.interrupt_predecessor_turn_id = effect.turn_id
           AND successor.defaults_version = NEW.expected_defaults_version
           AND cancelled.state_kind = 'terminal'
           AND cancelled.terminal_attempt_id = effect.yielded_turn_attempt_id
           AND cancelled.terminal_model_call_id IS NULL
           AND (
                (
                    effect.interrupted_tool_attempt_id IS NULL
                    AND cancelled.terminal_disposition_kind = 'cancelled'
                    AND cancelled.terminal_tool_attempt_id IS NULL
                )
                OR (
                    effect.interrupted_tool_attempt_id IS NOT NULL
                    AND cancelled.terminal_disposition_kind =
                        'reconciliation_required'
                    AND cancelled.terminal_tool_attempt_id =
                        effect.interrupted_tool_attempt_id
                    AND EXISTS (
                        SELECT 1
                          FROM tool_attempt AS stopped_tool
                          JOIN runner_physical_attempt_lease_binding AS binding
                            ON binding.attempt_id = stopped_tool.attempt_id
                          JOIN runner_lease_generation AS lease
                            ON lease.lease_id = binding.lease_id
                           AND lease.attempt_id = stopped_tool.attempt_id
                           AND lease.session_id = stopped_tool.session_id
                          JOIN runner_current_lease_event AS lease_head
                            ON lease_head.lease_id = lease.lease_id
                           AND lease_head.generation = lease.generation
                          JOIN runner_lease_event AS lease_event
                            ON lease_event.lease_id = lease_head.lease_id
                           AND lease_event.generation = lease_head.generation
                           AND lease_event.event_ordinal = lease_head.event_ordinal
                          JOIN runner_session_placement_record AS leased_placement
                            ON leased_placement.session_id = lease.session_id
                           AND leased_placement.event_ordinal =
                                lease.placement_event_ordinal
                         WHERE stopped_tool.attempt_id =
                                effect.interrupted_tool_attempt_id
                           AND stopped_tool.session_id = effect.session_id
                           AND stopped_tool.turn_id = effect.turn_id
                           AND stopped_tool.state_kind = 'terminal'
                           AND stopped_tool.terminal_disposition_kind = 'ambiguous'
                           AND lease.runner_id = effect.runner_id
                           AND lease_event.state_kind IN (
                                'lost_execution_possible', 'lost_claimed'
                           )
                           AND lease.effect_class IN ('idempotent', 'side_effecting')
                           AND leased_placement.placement_revision =
                                effect.placement_revision
                           AND leased_placement.state_kind = 'pinned'
                           AND leased_placement.pinned_runner_id = effect.runner_id
                    )
                )
                OR (
                    effect.interrupted_tool_attempt_id IS NOT NULL
                    AND cancelled.terminal_disposition_kind = 'cancelled'
                    AND cancelled.terminal_tool_attempt_id IS NULL
                    AND EXISTS (
                        SELECT 1
                          FROM tool_attempt AS stopped_tool
                         WHERE stopped_tool.attempt_id =
                                effect.interrupted_tool_attempt_id
                           AND stopped_tool.session_id = effect.session_id
                           AND stopped_tool.turn_id = effect.turn_id
                           AND stopped_tool.state_kind = 'terminal'
                           AND stopped_tool.terminal_disposition_kind = 'known_failed'
                           AND stopped_tool.error_kind = 'crash_lost'
                           AND stopped_tool.error_detail IS NULL
                    )
                )
           )
           AND yielded_attempt.state_kind = 'ended'
           AND yielded_attempt.end_variant = 'without_stop'
           AND yielded_attempt.end_disposition = 'yielded_to_durable_wait'
           AND yielded_attempt.interrupt_command_id IS NULL
           AND yielded_attempt.interrupt_predecessor_turn_id IS NULL;
    ELSIF NEW.result_kind = 'applied' THEN
        SELECT count(*)
          INTO matching_records
          FROM accepted_input AS accepted
          JOIN queued_input_origin AS successor
            ON successor.accepted_input_id = accepted.accepted_input_id
           AND successor.turn_id = accepted.origin_turn_id
           AND successor.session_id = accepted.session_id
           AND successor.acceptance_position = accepted.acceptance_position
          JOIN turn_attempt AS stopped_attempt
            ON stopped_attempt.turn_id = NEW.expected_active_turn_id
           AND stopped_attempt.session_id = NEW.session_id
           AND (
                (
                    stopped_attempt.interrupt_command_id = NEW.command_id
                    AND stopped_attempt.interrupt_predecessor_turn_id =
                        NEW.expected_active_turn_id
                    AND (
                        stopped_attempt.state_kind = 'stop_requested'
                        OR (
                            stopped_attempt.state_kind = 'ended'
                            AND stopped_attempt.end_variant = 'after_cancellation'
                        )
                    )
                )
                OR (
                    stopped_attempt.state_kind = 'ended'
                    AND stopped_attempt.end_variant = 'without_stop'
                    AND stopped_attempt.end_disposition IN ('ambiguous', 'lost')
                    AND stopped_attempt.interrupt_command_id IS NULL
                    AND stopped_attempt.interrupt_predecessor_turn_id IS NULL
                    AND EXISTS (
                        SELECT 1
                          FROM turn_lifecycle AS reconciled
                         WHERE reconciled.turn_id = stopped_attempt.turn_id
                           AND reconciled.session_id = stopped_attempt.session_id
                           AND reconciled.state_kind = 'terminal'
                           AND reconciled.terminal_disposition_kind =
                                'reconciliation_required'
                           AND reconciled.terminal_attempt_id =
                                stopped_attempt.turn_attempt_id
                    )
                )
                OR (
                    stopped_attempt.state_kind = 'ended'
                    AND stopped_attempt.end_variant = 'without_stop'
                    AND stopped_attempt.end_disposition = 'yielded_to_durable_wait'
                    AND stopped_attempt.interrupt_command_id IS NULL
                    AND stopped_attempt.interrupt_predecessor_turn_id IS NULL
                    AND EXISTS (
                        SELECT 1
                          FROM session_delegation_wait AS waiting
                          JOIN tool_request AS awaiting
                            ON awaiting.request_id = waiting.awaiting_tool_request_id
                           AND awaiting.turn_id = waiting.parent_turn_id
                           AND awaiting.session_id = waiting.parent_session_id
                          JOIN model_call AS producing_call
                            ON producing_call.model_call_id =
                                awaiting.producing_model_call_id
                           AND producing_call.turn_id = awaiting.turn_id
                           AND producing_call.session_id = awaiting.session_id
                          JOIN turn_lifecycle AS cancelled
                            ON cancelled.turn_id = waiting.parent_turn_id
                           AND cancelled.session_id = waiting.parent_session_id
                         WHERE waiting.parent_turn_id = NEW.expected_active_turn_id
                           AND waiting.parent_session_id = NEW.session_id
                           AND waiting.wait_mode = 'foreground'
                           AND producing_call.turn_attempt_id =
                                stopped_attempt.turn_attempt_id
                           AND cancelled.state_kind = 'terminal'
                           AND cancelled.terminal_disposition_kind = 'cancelled'
                           AND cancelled.terminal_attempt_id IS NULL
                           AND cancelled.terminal_model_call_id IS NULL
                    )
                )
           )
         WHERE accepted.accepting_command_id = NEW.command_id
           AND accepted.accepted_input_id = NEW.result_accepted_input_id
           AND accepted.session_id = NEW.result_session_id
           AND accepted_input_parts_match_command(accepted.accepted_input_id)
           AND accepted.delivery_kind = 'interrupt'
           AND accepted.expected_active_turn_id = NEW.expected_active_turn_id
           AND accepted.expected_defaults_version = NEW.expected_defaults_version
           AND accepted.model_override_kind = NEW.model_override_kind
           AND accepted.replacement_model_kind
               IS NOT DISTINCT FROM NEW.replacement_model_kind
           AND accepted.replacement_direct_model_selection_id
               IS NOT DISTINCT FROM NEW.replacement_direct_model_selection_id
           AND accepted.replacement_model_alias_id
               IS NOT DISTINCT FROM NEW.replacement_model_alias_id
           AND accepted.disposition_kind = 'origin_of'
           AND accepted.origin_turn_id = NEW.result_turn_id
           AND successor.priority_kind = 'interrupt_immediately_after'
           AND successor.interrupt_predecessor_turn_id = NEW.expected_active_turn_id
           AND successor.defaults_version = NEW.expected_defaults_version;
    ELSIF NEW.rejection_kind = 'interrupt_unavailable_while_awaiting_approval'
    THEN
        SELECT count(*)
          INTO matching_records
          FROM turn_lifecycle AS parked
         WHERE parked.turn_id = NEW.result_actual_active_turn_id
           AND parked.session_id = NEW.result_session_id
           AND parked.state_kind = 'active'
           AND parked.active_phase_kind = 'awaiting_tool_approval'
           AND NOT EXISTS (
                SELECT 1
                  FROM accepted_input
                 WHERE accepting_command_id = NEW.command_id
           );
    ELSE
        SELECT count(*)
          INTO matching_records
          FROM submit_input_command AS existing
          JOIN accepted_input AS accepted
            ON accepted.accepting_command_id = existing.command_id
           AND accepted.accepted_input_id = existing.result_accepted_input_id
           AND accepted.session_id = existing.result_session_id
           AND accepted.origin_turn_id = existing.result_turn_id
          JOIN queued_input_origin AS successor
            ON successor.accepted_input_id = accepted.accepted_input_id
           AND successor.turn_id = accepted.origin_turn_id
           AND successor.session_id = accepted.session_id
           AND successor.priority_kind = 'interrupt_immediately_after'
           AND successor.interrupt_predecessor_turn_id = NEW.result_actual_active_turn_id
          JOIN turn_lifecycle AS active
            ON active.turn_id = NEW.result_actual_active_turn_id
           AND active.session_id = NEW.result_session_id
           AND active.state_kind = 'active'
          JOIN turn_attempt AS stopped_attempt
            ON stopped_attempt.turn_attempt_id = active.current_attempt_id
           AND stopped_attempt.turn_id = active.turn_id
           AND stopped_attempt.session_id = active.session_id
           AND stopped_attempt.interrupt_command_id = existing.command_id
           AND stopped_attempt.interrupt_predecessor_turn_id = active.turn_id
           AND (
                (
                    active.active_phase_kind = 'running'
                    AND stopped_attempt.state_kind = 'stop_requested'
                )
                OR (
                    active.active_phase_kind = 'awaiting_model_call_recovery'
                    AND stopped_attempt.state_kind = 'ended'
                    AND stopped_attempt.end_variant = 'after_cancellation'
                    AND stopped_attempt.end_disposition IN ('ambiguous', 'lost')
                )
           )
         WHERE existing.command_id = NEW.result_existing_interrupt_command_id
           AND existing.result_kind = 'applied'
           AND existing.rejection_kind IS NULL
           AND existing.delivery_kind = 'interrupt'
           AND existing.expected_active_turn_id = NEW.result_actual_active_turn_id
           AND NOT EXISTS (
                SELECT 1
                  FROM accepted_input
                 WHERE accepting_command_id = NEW.command_id
           );
    END IF;

    IF matching_records <> 1 THEN
        RAISE EXCEPTION
            'interrupt submit-input command % has an incomplete or cross-wired effect',
            NEW.command_id
            USING ERRCODE = '23503';
    END IF;
    RETURN NULL;
END;
$$;


--
-- Name: require_pending_steering_active_source(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_pending_steering_active_source() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    checked_state text;
BEGIN
    IF TG_TABLE_NAME = 'accepted_input' THEN
        IF NEW.disposition_kind = 'pending_steering' THEN
            SELECT state_kind
              INTO checked_state
              FROM turn_lifecycle
             WHERE turn_id = NEW.expected_active_turn_id
               AND session_id = NEW.session_id
               FOR UPDATE;

            IF checked_state IS DISTINCT FROM 'active' THEN
                RAISE EXCEPTION
                    'pending steering % does not name an active source turn',
                    NEW.accepted_input_id
                    USING
                        ERRCODE = '23514',
                        CONSTRAINT = 'accepted_input_pending_source_active';
            END IF;
        END IF;
    ELSIF NEW.state_kind = 'terminal'
          AND EXISTS (
              SELECT 1
                FROM accepted_input
               WHERE session_id = NEW.session_id
                 AND expected_active_turn_id = NEW.turn_id
                 AND disposition_kind = 'pending_steering'
          )
    THEN
        RAISE EXCEPTION
            'turn % cannot become terminal while pending steering remains',
            NEW.turn_id
            USING
                ERRCODE = '23514',
                CONSTRAINT = 'turn_lifecycle_pending_steering_closed';
    END IF;

    RETURN NULL;
END;
$$;


--
-- Name: require_semantic_entry_turn_state(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_semantic_entry_turn_state() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    entry semantic_transcript_entry%ROWTYPE;
    checked_turn_id uuid;
    checked_producing_call_id uuid;
BEGIN
    IF TG_OP = 'DELETE' THEN
        entry := OLD;
    ELSE
        entry := NEW;
    END IF;
    checked_producing_call_id := entry.producing_model_call_id;

    IF entry.payload_kind = 'imported_entry' THEN
        RETURN NULL;
    END IF;

    IF entry.payload_kind = 'context_summary' THEN
        SELECT model_call_id
          INTO checked_producing_call_id
          FROM context_compaction_model_call
         WHERE model_call_id = entry.context_summary_producing_call_id
           AND session_id = entry.source_session_id
           AND state_kind = 'terminal'
           AND terminal_disposition_kind = 'completed';
        IF NOT FOUND THEN
            RAISE EXCEPTION 'context summary lacks its completed dedicated call'
                USING ERRCODE = '23514';
        END IF;
        RETURN NULL;
    END IF;

    CASE entry.payload_kind
        WHEN 'model_identity_changed' THEN
            SELECT origin.turn_id
              INTO checked_turn_id
              FROM queued_input_origin AS origin
              JOIN LATERAL turn_origin_effective_model_configuration(
                   origin.turn_id,
                   origin.session_id
              ) AS effective
                ON true
             WHERE origin.turn_id = entry.model_identity_turn_id
               AND origin.session_id = entry.source_session_id
               AND effective.defaults_version =
                   entry.model_identity_defaults_version
               AND effective.direct_selection_id =
                   entry.model_identity_direct_selection_id;
        WHEN 'origin_accepted_input' THEN
            SELECT origin_turn_id
              INTO checked_turn_id
              FROM accepted_input
             WHERE accepted_input_id = entry.origin_accepted_input_id
               AND session_id = entry.source_session_id
               AND disposition_kind IN (
                    'origin_of',
                    'reclassified_as_turn_origin'
               )
               AND origin_turn_id IS NOT NULL;
            IF NOT FOUND THEN
                RAISE EXCEPTION 'semantic origin input is not a turn origin'
                    USING
                        ERRCODE = '23514',
                        CONSTRAINT =
                            'semantic_transcript_entry_origin_disposition';
            END IF;
        WHEN 'steering_accepted_input' THEN
            SELECT expected_active_turn_id, consuming_model_call_id
              INTO checked_turn_id, checked_producing_call_id
              FROM accepted_input
             WHERE accepted_input_id = entry.origin_accepted_input_id
               AND session_id = entry.source_session_id
               AND disposition_kind = 'consumed_as_steering'
               AND expected_active_turn_id =
                   entry.steering_source_turn_id
               AND consuming_model_call_id IS NOT NULL;
            IF NOT FOUND THEN
                RAISE EXCEPTION
                    'semantic steering input lacks consuming call'
                    USING ERRCODE = '23514';
            END IF;
        WHEN 'turn_failed' THEN
            checked_turn_id := entry.failed_turn_id;
        WHEN 'turn_completed' THEN
            checked_turn_id := entry.completed_turn_id;
        WHEN 'turn_cancelled' THEN
            checked_turn_id := entry.cancelled_turn_id;
        WHEN 'assistant_text' THEN
            SELECT turn_id
              INTO checked_turn_id
              FROM model_call
             WHERE model_call_id = entry.producing_model_call_id
               AND state_kind = 'terminal'
               AND terminal_disposition_kind = 'completed';
        WHEN 'assistant_tool_use' THEN
            SELECT request.turn_id
              INTO checked_turn_id
              FROM tool_request AS request
             WHERE request.request_id = entry.assistant_tool_request_id
               AND request.producing_model_call_id =
                   entry.producing_model_call_id
               AND request.session_id = entry.source_session_id;
        WHEN 'tool_execution_result' THEN
            SELECT turn_id
              INTO checked_turn_id
              FROM tool_attempt
             WHERE attempt_id = entry.tool_result_attempt_id
               AND session_id = entry.source_session_id
               AND state_kind = 'terminal'
               AND terminal_disposition_kind IN ('completed', 'known_failed');
        WHEN 'tool_denied' THEN
            SELECT request.turn_id
              INTO checked_turn_id
              FROM tool_request AS request
              JOIN tool_approval_decision AS approval
                ON approval.request_id = request.request_id
               AND approval.decision_kind = 'deny'
             WHERE request.request_id = entry.tool_result_request_id
               AND request.session_id = entry.source_session_id;
        WHEN 'tool_closed_by_turn_end' THEN
            SELECT request.turn_id
              INTO checked_turn_id
              FROM tool_request AS request
              JOIN turn_lifecycle AS lifecycle
                ON lifecycle.turn_id = request.turn_id
               AND lifecycle.session_id = request.session_id
               AND lifecycle.state_kind = 'terminal'
             WHERE request.request_id = entry.tool_result_request_id
               AND request.session_id = entry.source_session_id;
        ELSE
            RAISE EXCEPTION
                'semantic payload kind % lacks construction authority',
                entry.payload_kind
                USING ERRCODE = '23514';
    END CASE;

    IF checked_turn_id IS NULL THEN
        RAISE EXCEPTION 'semantic entry lacks authoritative turn'
            USING ERRCODE = '23514';
    END IF;
    PERFORM assert_turn_lifecycle_final_state(checked_turn_id);
    IF checked_producing_call_id IS NOT NULL THEN
        PERFORM assert_model_call_final_state(checked_producing_call_id);
    END IF;
    RETURN NULL;
END;
$$;


--
-- Name: require_semantic_steering_final_state(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_semantic_steering_final_state() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    checked_kind text;
    checked_accepted_input uuid;
BEGIN
    checked_kind := CASE WHEN TG_OP = 'DELETE' THEN OLD.payload_kind ELSE NEW.payload_kind END;
    checked_accepted_input := CASE
        WHEN TG_OP = 'DELETE' THEN OLD.origin_accepted_input_id
        ELSE NEW.origin_accepted_input_id
    END;
    IF checked_kind = 'steering_accepted_input' THEN
        PERFORM assert_steering_accepted_input_final_state(checked_accepted_input);
    END IF;
    RETURN NULL;
END;
$$;


--
-- Name: require_submit_input_command_parts(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_submit_input_command_parts() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE checked_command uuid;
BEGIN
    checked_command := CASE WHEN TG_TABLE_NAME = 'submit_input_command'
        THEN NEW.command_id ELSE COALESCE(NEW.command_id, OLD.command_id) END;
    IF NOT submit_input_command_parts_are_valid(checked_command) THEN
        RAISE EXCEPTION 'submit-input command has invalid ordered content parts'
            USING ERRCODE = '23514',
                CONSTRAINT = 'submit_input_command_content_parts_valid';
    END IF;
    RETURN NULL;
END;
$$;


--
-- Name: require_submit_input_legacy_effect_correlation(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_submit_input_legacy_effect_correlation() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    matching_records bigint;
BEGIN
    IF NEW.result_kind = 'applied' AND NEW.result_turn_id IS NOT NULL THEN
        SELECT count(*)
          INTO matching_records
          FROM accepted_input AS accepted
          JOIN queued_input_origin AS queued
            ON queued.accepted_input_id = accepted.accepted_input_id
           AND queued.session_id = accepted.session_id
           AND queued.acceptance_position = accepted.acceptance_position
           AND queued.turn_id = accepted.origin_turn_id
          JOIN session_defaults_version AS defaults
            ON defaults.session_id = queued.session_id
           AND defaults.version = queued.defaults_version
         WHERE accepted.accepting_command_id = NEW.command_id
           AND accepted.accepted_input_id = NEW.result_accepted_input_id
           AND accepted.session_id = NEW.result_session_id
           AND accepted_input_parts_match_command(accepted.accepted_input_id)
           AND accepted.delivery_kind = NEW.delivery_kind
           AND accepted.expected_active_turn_id
               IS NOT DISTINCT FROM NEW.expected_active_turn_id
           AND accepted.expected_defaults_version = NEW.expected_defaults_version
           AND accepted.model_override_kind = NEW.model_override_kind
           AND accepted.replacement_model_kind
               IS NOT DISTINCT FROM NEW.replacement_model_kind
           AND accepted.replacement_direct_model_selection_id
               IS NOT DISTINCT FROM NEW.replacement_direct_model_selection_id
           AND accepted.replacement_model_alias_id
               IS NOT DISTINCT FROM NEW.replacement_model_alias_id
           AND accepted.disposition_kind = 'origin_of'
           AND accepted.origin_turn_id = NEW.result_turn_id
           AND queued.priority_kind = 'ordinary'
           AND queued.defaults_version = NEW.expected_defaults_version
           AND (
               (
                   NEW.model_override_kind = 'use_session_default'
                   AND queued.requested_model_kind = defaults.model_selection_kind
                   AND queued.requested_direct_model_selection_id
                       IS NOT DISTINCT FROM defaults.direct_model_selection_id
                   AND queued.requested_model_alias_id
                       IS NOT DISTINCT FROM defaults.model_alias_id
               )
               OR
               (
                   NEW.model_override_kind = 'replace_with'
                   AND queued.requested_model_kind = NEW.replacement_model_kind
                   AND queued.requested_direct_model_selection_id
                       IS NOT DISTINCT FROM
                           NEW.replacement_direct_model_selection_id
                   AND queued.requested_model_alias_id
                       IS NOT DISTINCT FROM NEW.replacement_model_alias_id
               )
           )
           AND (
               (
                   queued.requested_model_kind = 'direct'
                   AND queued.frozen_model_kind = 'direct'
                   AND queued.frozen_direct_model_selection_id
                       = queued.requested_direct_model_selection_id
               )
               OR
               (
                   queued.requested_model_kind = 'alias'
                   AND queued.frozen_model_kind = 'frozen_alias'
                   AND queued.frozen_model_alias_id = queued.requested_model_alias_id
               )
           )
           AND queued.model_parameters = 'provider_defaults'
           AND queued.known_provider_failure_retry = 'disabled'
           AND queued.model_fallback = 'disabled';
    ELSIF NEW.result_kind = 'applied' THEN
        SELECT count(*)
          INTO matching_records
          FROM accepted_input AS accepted
         WHERE accepted.accepting_command_id = NEW.command_id
           AND accepted.accepted_input_id = NEW.result_accepted_input_id
           AND accepted.session_id = NEW.result_session_id
           AND accepted_input_parts_match_command(accepted.accepted_input_id)
           AND accepted.delivery_kind = 'next_safe_point'
           AND accepted.delivery_kind = NEW.delivery_kind
           AND accepted.expected_active_turn_id = NEW.expected_active_turn_id
           AND accepted.expected_defaults_version IS NULL
           AND accepted.model_override_kind IS NULL
           AND accepted.replacement_model_kind IS NULL
           AND accepted.replacement_direct_model_selection_id IS NULL
           AND accepted.replacement_model_alias_id IS NULL
           AND accepted.disposition_kind = 'pending_steering'
           AND accepted.origin_turn_id IS NULL
           AND accepted.expected_active_turn_id = NEW.result_actual_active_turn_id
           AND NOT EXISTS (
               SELECT 1
                 FROM queued_input_origin
                WHERE accepted_input_id = accepted.accepted_input_id
           );
    ELSE
        SELECT count(*)
          INTO matching_records
          FROM accepted_input
         WHERE accepting_command_id = NEW.command_id;

        IF matching_records = 0
           AND NEW.rejection_kind = 'unknown_model_alias'
        THEN
            SELECT count(*)
              INTO matching_records
              FROM session_defaults_version AS defaults
             WHERE defaults.session_id = NEW.result_session_id
               AND defaults.version = NEW.result_selected_defaults_version
               AND (
                   (
                       NEW.model_override_kind = 'use_session_default'
                       AND defaults.model_selection_kind = 'alias'
                       AND defaults.model_alias_id = NEW.result_unknown_alias_id
                   )
                   OR
                   (
                       NEW.model_override_kind = 'replace_with'
                       AND NEW.replacement_model_kind = 'alias'
                       AND NEW.replacement_model_alias_id = NEW.result_unknown_alias_id
                   )
               );

            IF matching_records <> 1 THEN
                RAISE EXCEPTION
                    'submit-input command % has cross-wired unknown-alias evidence',
                    NEW.command_id
                    USING ERRCODE = '23503';
            END IF;
            matching_records := 0;
        END IF;

        IF matching_records = 0
           AND NEW.rejection_kind IN (
               'session_defaults_version_mismatch',
               'unknown_model_alias',
               'acceptance_position_exhausted'
           )
           AND NEW.delivery_kind IN ('after_current_turn', 'next_safe_point')
        THEN
            SELECT count(*)
              INTO matching_records
              FROM turn_lifecycle AS turn
              JOIN queued_input_origin AS queued
                ON queued.turn_id = turn.turn_id
               AND queued.session_id = turn.session_id
               AND queued.accepted_input_id = turn.origin_accepted_input_id
              JOIN accepted_input AS accepted
                ON accepted.accepted_input_id = queued.accepted_input_id
               AND accepted.session_id = turn.session_id
               AND accepted.origin_turn_id = turn.turn_id
               AND accepted.disposition_kind = 'origin_of'
             WHERE turn.turn_id = NEW.expected_active_turn_id
               AND turn.session_id = NEW.result_session_id;

            IF matching_records <> 1 THEN
                RAISE EXCEPTION
                    'submit-input rejection % has cross-wired source-turn evidence',
                    NEW.command_id
                    USING ERRCODE = '23503',
                        CONSTRAINT =
                            'submit_input_command_rejected_source_origin';
            END IF;
            matching_records := 0;
        END IF;
    END IF;

    IF matching_records <> (
        CASE WHEN NEW.result_kind = 'applied' THEN 1 ELSE 0 END
    ) THEN
        RAISE EXCEPTION
            'submit-input command % has an incomplete or cross-wired terminal effect',
            NEW.command_id
            USING ERRCODE = '23503';
    END IF;

    RETURN NULL;
END;
$$;


--
-- Name: require_turn_attempt_final_state(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_turn_attempt_final_state() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    checked_attempt_id uuid;
    checked_turn_id uuid;
BEGIN
    checked_attempt_id :=
        CASE WHEN TG_OP = 'DELETE' THEN OLD.turn_attempt_id ELSE NEW.turn_attempt_id END;
    checked_turn_id :=
        CASE WHEN TG_OP = 'DELETE' THEN OLD.turn_id ELSE NEW.turn_id END;

    PERFORM assert_turn_attempt_final_state(checked_attempt_id);
    PERFORM assert_turn_lifecycle_final_state(checked_turn_id);
    RETURN NULL;
END;
$$;


--
-- Name: require_turn_child_wait_mode(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_turn_child_wait_mode() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NEW.active_phase_kind = 'awaiting_child' AND NOT EXISTS (
        SELECT 1 FROM session_delegation_wait
         WHERE awaiting_tool_request_id = NEW.child_wait_request_id
           AND parent_turn_id = NEW.turn_id
           AND parent_session_id = NEW.session_id
           AND wait_mode = 'foreground'
    ) THEN
        RAISE EXCEPTION 'child-wait turn phase requires an exact foreground wait'
            USING ERRCODE = '23514',
                CONSTRAINT = 'turn_lifecycle_child_wait_mode';
    END IF;
    RETURN NULL;
END;
$$;


--
-- Name: require_turn_delegation_runtime_terminal_proof(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_turn_delegation_runtime_terminal_proof() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NEW.delegation_runtime_terminal AND NOT EXISTS (
        SELECT 1 FROM session_delegation_logical_terminal AS terminal
         WHERE terminal.child_session_id = NEW.session_id
           AND terminal.child_turn_id = NEW.turn_id
    ) THEN
        RAISE EXCEPTION 'released delegation runtime slot lacks terminal proof'
            USING ERRCODE = '23503',
                CONSTRAINT = 'turn_lifecycle_delegation_runtime_terminal_proof';
    END IF;
    RETURN NULL;
END;
$$;


--
-- Name: require_turn_lifecycle_final_state(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_turn_lifecycle_final_state() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    PERFORM assert_turn_lifecycle_final_state(
        CASE WHEN TG_OP = 'DELETE' THEN OLD.turn_id ELSE NEW.turn_id END
    );
    RETURN NULL;
END;
$$;


--
-- Name: require_turn_runner_recovery_complete(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_turn_runner_recovery_complete() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF TG_OP <> 'DELETE' THEN
        PERFORM assert_turn_runner_recovery_complete(NEW.session_id, NEW.turn_id);
    END IF;
    IF TG_OP <> 'INSERT'
       AND (TG_OP = 'DELETE'
            OR ROW(OLD.session_id, OLD.turn_id) IS DISTINCT FROM
               ROW(NEW.session_id, NEW.turn_id))
    THEN
        PERFORM assert_turn_runner_recovery_complete(OLD.session_id, OLD.turn_id);
    END IF;
    RETURN NULL;
END;
$$;


--
-- Name: submit_input_command_content_parts_json(uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION submit_input_command_content_parts_json(checked_command uuid) RETURNS jsonb
    LANGUAGE sql STABLE
    AS $$
    SELECT COALESCE(
        jsonb_agg(
            jsonb_build_object(
                'position', position,
                'part_kind', part_kind,
                'text_value', text_value,
                'blob_digest', CASE
                    WHEN blob_digest IS NULL THEN NULL
                    ELSE 'sha256:' || encode(blob_digest, 'hex')
                END,
                'attachment_kind', attachment_kind,
                'declared_media_type', declared_media_type,
                'display_filename', display_filename
            ) ORDER BY position
        ),
        '[]'::jsonb
    )
      FROM submit_input_command_content_part
     WHERE command_id = checked_command;
$$;


--
-- Name: submit_input_command_parts_are_valid(uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION submit_input_command_parts_are_valid(checked_command uuid) RETURNS boolean
    LANGUAGE sql STABLE
    AS $$
    SELECT count(*) BETWEEN 1 AND 256
       AND min(position) = 0
       AND max(position) = count(*) - 1
       AND COALESCE(sum(octet_length(convert_to(text_value, 'UTF8')))
            FILTER (WHERE part_kind = 'text'), 0) <= 1048576
       AND NOT EXISTS (
            SELECT 1
              FROM submit_input_command_content_part AS current
              JOIN submit_input_command_content_part AS prior
                ON prior.command_id = current.command_id
               AND prior.position + 1 = current.position
             WHERE current.command_id = checked_command
               AND current.part_kind = 'text'
               AND prior.part_kind = 'text')
      FROM submit_input_command_content_part
     WHERE command_id = checked_command;
$$;


--
-- Name: turn_lifecycle_effective_terminal_frontier(uuid, uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION turn_lifecycle_effective_terminal_frontier(checked_session_id uuid, checked_turn_id uuid) RETURNS uuid
    LANGUAGE sql STABLE
    AS $$
    SELECT CASE lifecycle.state_kind
        WHEN 'terminal' THEN lifecycle.terminal_frontier_id
        ELSE logical_terminal.terminal_frontier_id
    END
      FROM turn_lifecycle AS lifecycle
      LEFT JOIN session_delegation_logical_terminal AS logical_terminal
        ON logical_terminal.child_session_id = lifecycle.session_id
       AND logical_terminal.child_turn_id = lifecycle.turn_id
     WHERE lifecycle.session_id = checked_session_id
       AND lifecycle.turn_id = checked_turn_id
       AND (
            lifecycle.state_kind = 'terminal'
            OR lifecycle.delegation_runtime_terminal
       )
$$;


--
-- Name: turn_lifecycle_origin_member_span(uuid, uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION turn_lifecycle_origin_member_span(checked_turn_id uuid, checked_session_id uuid) RETURNS numeric
    LANGUAGE sql STABLE
    AS $$
    SELECT COALESCE(
        (
            SELECT wake.through_delivery_sequence
                   - wake.first_delivery_sequence + 1
              FROM session_delegation_wake_turn_origin AS wake
             WHERE wake.turn_id = checked_turn_id
               AND wake.recipient_session_id = checked_session_id
        ),
        1
    )
$$;


--
-- Name: turn_lifecycle_origin_semantic_entry(uuid, uuid, uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION turn_lifecycle_origin_semantic_entry(checked_turn_id uuid, checked_session_id uuid, checked_origin_input_id uuid) RETURNS TABLE(semantic_entry_id uuid)
    LANGUAGE sql STABLE
    AS $$
    SELECT entry.semantic_entry_id
      FROM turn_lifecycle AS lifecycle
      JOIN semantic_transcript_entry AS entry
        ON entry.source_session_id = lifecycle.session_id
      LEFT JOIN session_delegation_initial_task AS task
        ON task.turn_id = lifecycle.turn_id
       AND task.child_session_id = lifecycle.session_id
       AND task.semantic_entry_id = entry.semantic_entry_id
      LEFT JOIN session_delegation_wake_turn_origin AS wake
        ON wake.turn_id = lifecycle.turn_id
       AND wake.recipient_session_id = lifecycle.session_id
       AND delegation_delivery_semantic_entry(
            wake.recipient_session_id,
            wake.through_delivery_sequence
       ) = entry.semantic_entry_id
     WHERE lifecycle.turn_id = checked_turn_id
       AND lifecycle.session_id = checked_session_id
       AND (
            (lifecycle.origin_kind = 'accepted_input'
                AND entry.payload_kind = 'origin_accepted_input'
                AND entry.origin_accepted_input_id = checked_origin_input_id)
            OR (lifecycle.origin_kind = 'delegation'
                AND lifecycle.state_kind <> 'queued'
                AND (
                    (entry.payload_kind = 'delegated_task'
                        AND task.spawning_tool_request_id =
                            entry.delegated_task_spawning_tool_request_id)
                    OR wake.turn_id IS NOT NULL
                ))
       )
$$;


--
-- Name: turn_origin_effective_model_configuration(uuid, uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION turn_origin_effective_model_configuration(checked_turn_id uuid, checked_session_id uuid) RETURNS TABLE(defaults_version numeric, direct_selection_id uuid)
    LANGUAGE sql STABLE
    AS $$
    WITH RECURSIVE configuration_chain AS (
        (
            SELECT
                origin.turn_id,
                origin.source_configuration_turn_id,
                origin.defaults_version,
                COALESCE(
                    origin.frozen_direct_model_selection_id,
                    origin.frozen_alias_selected_direct_id
                ) AS direct_selection_id,
                ARRAY[origin.turn_id]::uuid[] AS visited_turn_ids
              FROM queued_input_origin AS origin
             WHERE origin.turn_id = checked_turn_id
               AND origin.session_id = checked_session_id

            UNION ALL

            SELECT
                task.turn_id,
                NULL::uuid AS source_configuration_turn_id,
                task.defaults_version,
                COALESCE(
                    task.frozen_direct_model_selection_id,
                    task.frozen_alias_selected_direct_id
                ),
                ARRAY[task.turn_id]::uuid[] AS visited_turn_ids
              FROM session_delegation_initial_task AS task
             WHERE task.turn_id = checked_turn_id
               AND task.child_session_id = checked_session_id

            UNION ALL

            SELECT
                wake.turn_id,
                NULL::uuid AS source_configuration_turn_id,
                wake.defaults_version,
                COALESCE(
                    wake.frozen_direct_model_selection_id,
                    wake.frozen_alias_selected_direct_id
                ),
                ARRAY[wake.turn_id]::uuid[] AS visited_turn_ids
              FROM session_delegation_wake_turn_origin AS wake
             WHERE wake.turn_id = checked_turn_id
               AND wake.recipient_session_id = checked_session_id
        )

        UNION ALL

        SELECT
            source.turn_id,
            source.source_configuration_turn_id,
            source.defaults_version,
            COALESCE(
                source.frozen_direct_model_selection_id,
                source.frozen_alias_selected_direct_id
            ),
            chain.visited_turn_ids || source.turn_id
          FROM configuration_chain AS chain
          JOIN queued_input_origin AS source
            ON source.turn_id = chain.source_configuration_turn_id
           AND source.session_id = checked_session_id
         WHERE NOT source.turn_id = ANY(chain.visited_turn_ids)
    )
    SELECT chain.defaults_version, chain.direct_selection_id
      FROM configuration_chain AS chain
     WHERE chain.defaults_version IS NOT NULL
       AND chain.direct_selection_id IS NOT NULL
     ORDER BY cardinality(chain.visited_turn_ids) DESC
     LIMIT 1
$$;


--
-- Name: turn_origin_exact_model_configuration(uuid, uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION turn_origin_exact_model_configuration(checked_turn_id uuid, checked_session_id uuid) RETURNS TABLE(defaults_version numeric, requested_model_kind text, requested_direct_model_selection_id uuid, requested_model_alias_id uuid, frozen_model_kind text, frozen_direct_model_selection_id uuid, frozen_model_alias_id uuid, frozen_alias_selected_direct_id uuid)
    LANGUAGE sql STABLE
    AS $$
    WITH RECURSIVE configuration_chain AS (
        (
            SELECT
                origin.turn_id,
                origin.source_configuration_turn_id,
                origin.defaults_version,
                origin.requested_model_kind,
                origin.requested_direct_model_selection_id,
                origin.requested_model_alias_id,
                origin.frozen_model_kind,
                origin.frozen_direct_model_selection_id,
                origin.frozen_model_alias_id,
                origin.frozen_alias_selected_direct_id,
                ARRAY[origin.turn_id]::uuid[] AS visited_turn_ids
              FROM queued_input_origin AS origin
             WHERE origin.turn_id = checked_turn_id
               AND origin.session_id = checked_session_id

            UNION ALL

            SELECT
                task.turn_id,
                NULL::uuid,
                task.defaults_version,
                task.requested_model_kind,
                task.requested_direct_model_selection_id,
                task.requested_model_alias_id,
                task.frozen_model_kind,
                task.frozen_direct_model_selection_id,
                task.frozen_model_alias_id,
                task.frozen_alias_selected_direct_id,
                ARRAY[task.turn_id]::uuid[]
              FROM session_delegation_initial_task AS task
             WHERE task.turn_id = checked_turn_id
               AND task.child_session_id = checked_session_id

            UNION ALL

            SELECT
                wake.turn_id,
                NULL::uuid,
                wake.defaults_version,
                wake.requested_model_kind,
                wake.requested_direct_model_selection_id,
                wake.requested_model_alias_id,
                wake.frozen_model_kind,
                wake.frozen_direct_model_selection_id,
                wake.frozen_model_alias_id,
                wake.frozen_alias_selected_direct_id,
                ARRAY[wake.turn_id]::uuid[]
              FROM session_delegation_wake_turn_origin AS wake
             WHERE wake.turn_id = checked_turn_id
               AND wake.recipient_session_id = checked_session_id
        )

        UNION ALL

        SELECT
            source.turn_id,
            source.source_configuration_turn_id,
            source.defaults_version,
            source.requested_model_kind,
            source.requested_direct_model_selection_id,
            source.requested_model_alias_id,
            source.frozen_model_kind,
            source.frozen_direct_model_selection_id,
            source.frozen_model_alias_id,
            source.frozen_alias_selected_direct_id,
            chain.visited_turn_ids || source.turn_id
          FROM configuration_chain AS chain
          JOIN queued_input_origin AS source
            ON source.turn_id = chain.source_configuration_turn_id
           AND source.session_id = checked_session_id
         WHERE NOT source.turn_id = ANY(chain.visited_turn_ids)
    )
    SELECT
        chain.defaults_version,
        chain.requested_model_kind,
        chain.requested_direct_model_selection_id,
        chain.requested_model_alias_id,
        chain.frozen_model_kind,
        chain.frozen_direct_model_selection_id,
        chain.frozen_model_alias_id,
        chain.frozen_alias_selected_direct_id
      FROM configuration_chain AS chain
     WHERE chain.defaults_version IS NOT NULL
       AND chain.requested_model_kind IS NOT NULL
       AND chain.frozen_model_kind IS NOT NULL
     ORDER BY cardinality(chain.visited_turn_ids) DESC
     LIMIT 1
$$;


--
-- Name: turn_start_effective_predecessor_frontier(uuid, uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION turn_start_effective_predecessor_frontier(checked_session uuid, checked_predecessor_frontier uuid) RETURNS TABLE(context_frontier_id uuid, member_count numeric)
    LANGUAGE sql STABLE
    AS $$
    WITH leaf AS MATERIALIZED (
        SELECT candidate.result_frontier_id
          FROM context_compaction AS candidate
         WHERE candidate.session_id = checked_session
           AND NOT EXISTS (
                SELECT 1
                  FROM context_compaction AS successor
                 WHERE successor.session_id = candidate.session_id
                   AND successor.predecessor_compaction_id =
                           candidate.context_compaction_id
           )
    ),
    applicable_leaf AS (
        SELECT leaf.result_frontier_id
          FROM leaf
          JOIN context_frontier AS candidate
            ON candidate.owning_session_id = checked_session
           AND candidate.context_frontier_id = leaf.result_frontier_id
          JOIN context_frontier AS predecessor
            ON predecessor.owning_session_id = checked_session
           AND predecessor.context_frontier_id =
                   checked_predecessor_frontier
         WHERE CASE
                   WHEN candidate.member_count < predecessor.member_count
                   THEN false
                   ELSE context_frontier_preserves_prefix(
                        checked_session,
                        checked_predecessor_frontier,
                        leaf.result_frontier_id
                   )
               END
    )
    SELECT frontier.context_frontier_id, frontier.member_count
      FROM context_frontier AS frontier
     WHERE frontier.owning_session_id = checked_session
       AND frontier.context_frontier_id = COALESCE(
            (SELECT result_frontier_id FROM applicable_leaf),
            checked_predecessor_frontier
       )
$$;


--
-- Name: turn_start_model_identity_boundary_is_valid(uuid, uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION turn_start_model_identity_boundary_is_valid(checked_turn_id uuid, checked_frontier_id uuid) RETURNS boolean
    LANGUAGE plpgsql STABLE
    AS $$
DECLARE
    checked_session uuid;
    checked_defaults_version numeric(20, 0);
    checked_selection uuid;
    boundary_required boolean;
    predecessor_turn uuid;
    predecessor_selection uuid;
    starting_member_count numeric(20, 0);
    boundary_entry_count bigint;
    boundary_member_count bigint;
    boundary_member_position numeric(20, 0);
BEGIN
    SELECT lifecycle.session_id, lifecycle.model_identity_boundary_required
      INTO checked_session, boundary_required
      FROM turn_lifecycle AS lifecycle
     WHERE lifecycle.turn_id = checked_turn_id
       AND (
            (
                lifecycle.origin_kind = 'accepted_input'
                AND EXISTS (
                    SELECT 1
                      FROM queued_input_origin AS origin
                     WHERE origin.turn_id = lifecycle.turn_id
                       AND origin.session_id = lifecycle.session_id
                )
            )
            OR (
                lifecycle.origin_kind = 'delegation'
                AND (
                    EXISTS (
                        SELECT 1
                          FROM session_delegation_initial_task AS task
                         WHERE task.turn_id = lifecycle.turn_id
                           AND task.child_session_id = lifecycle.session_id
                    )
                    OR EXISTS (
                        SELECT 1
                          FROM session_delegation_wake_turn_origin AS wake
                         WHERE wake.turn_id = lifecycle.turn_id
                           AND wake.recipient_session_id = lifecycle.session_id
                    )
                )
            )
       );

    IF NOT FOUND THEN
        RETURN false;
    END IF;

    SELECT
        effective.defaults_version,
        effective.direct_selection_id
      INTO checked_defaults_version, checked_selection
      FROM turn_origin_effective_model_configuration(
               checked_turn_id,
               checked_session
           ) AS effective;

    IF NOT FOUND THEN
        RETURN false;
    END IF;

    predecessor_turn := accepted_input_turn_queue_predecessor(
        checked_session,
        checked_turn_id
    );
    IF predecessor_turn IS NOT NULL THEN
        SELECT effective.direct_selection_id
          INTO predecessor_selection
          FROM turn_origin_effective_model_configuration(
                   predecessor_turn,
                   checked_session
               ) AS effective;
        IF NOT FOUND THEN
            RETURN false;
        END IF;
    END IF;

    SELECT count(*)
      INTO boundary_entry_count
      FROM semantic_transcript_entry AS entry
     WHERE entry.source_session_id = checked_session
       AND entry.payload_kind = 'model_identity_changed'
       AND entry.model_identity_turn_id = checked_turn_id
       AND entry.model_identity_defaults_version = checked_defaults_version
       AND entry.model_identity_direct_selection_id = checked_selection;

    IF NOT boundary_required THEN
        RETURN boundary_entry_count = 0;
    END IF;

    IF predecessor_turn IS NULL
       OR predecessor_selection IS NOT DISTINCT FROM checked_selection
    THEN
        RETURN boundary_entry_count = 0;
    END IF;

    SELECT member_count
      INTO starting_member_count
      FROM context_frontier
     WHERE owning_session_id = checked_session
       AND context_frontier_id = checked_frontier_id;

    SELECT count(*), max(member.member_position)
      INTO boundary_member_count, boundary_member_position
      FROM semantic_transcript_entry AS entry
      JOIN context_frontier_member AS member
        ON member.source_session_id = entry.source_session_id
       AND member.semantic_entry_id = entry.semantic_entry_id
     WHERE entry.source_session_id = checked_session
       AND entry.payload_kind = 'model_identity_changed'
       AND entry.model_identity_turn_id = checked_turn_id
       AND member.owning_session_id = checked_session
       AND member.context_frontier_id = checked_frontier_id;

    RETURN boundary_entry_count = 1
       AND boundary_member_count = 1
       AND boundary_member_position IS NOT DISTINCT FROM
           starting_member_count - 1;
END;
$$;


--
-- Name: turn_start_model_identity_entry_count(uuid, uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION turn_start_model_identity_entry_count(checked_turn_id uuid, checked_frontier_id uuid) RETURNS bigint
    LANGUAGE sql STABLE
    AS $$
    SELECT count(*)
      FROM semantic_transcript_entry AS entry
      JOIN context_frontier_member AS member
        ON member.source_session_id = entry.source_session_id
       AND member.semantic_entry_id = entry.semantic_entry_id
     WHERE entry.model_identity_turn_id = checked_turn_id
       AND member.context_frontier_id = checked_frontier_id
       AND member.owning_session_id = entry.source_session_id
$$;


--
-- Tables.
--

--
-- Name: accepted_input; Type: TABLE; Schema: public
--

CREATE TABLE accepted_input (
    accepted_input_id uuid NOT NULL,
    accepting_command_id uuid,
    session_id uuid NOT NULL,
    delivery_kind text NOT NULL,
    expected_active_turn_id uuid,
    expected_defaults_version numeric(20,0),
    model_override_kind text,
    replacement_model_kind text,
    replacement_direct_model_selection_id uuid,
    replacement_model_alias_id uuid,
    acceptance_position numeric(20,0) NOT NULL,
    disposition_kind text NOT NULL,
    origin_turn_id uuid,
    consuming_model_call_id uuid,
    descendant_scope text,
    model_settings_override jsonb DEFAULT '{"fast_mode": {"kind": "inherit"}, "service_tier": {"kind": "inherit"}, "reasoning_level": {"kind": "inherit"}}'::jsonb NOT NULL,
    content_parts_creation_xid xid8 DEFAULT pg_current_xact_id() NOT NULL,
    CONSTRAINT accepted_input_configuration_shape CHECK ((((model_override_kind = 'use_session_default'::text) AND (replacement_model_kind IS NULL) AND (replacement_direct_model_selection_id IS NULL) AND (replacement_model_alias_id IS NULL)) OR ((model_override_kind = 'replace_with'::text) AND (replacement_model_kind = 'direct'::text) AND (replacement_direct_model_selection_id IS NOT NULL) AND (replacement_model_alias_id IS NULL)) OR ((model_override_kind = 'replace_with'::text) AND (replacement_model_kind = 'alias'::text) AND (replacement_direct_model_selection_id IS NULL) AND (replacement_model_alias_id IS NOT NULL)))),
    CONSTRAINT accepted_input_delivery_shape CHECK ((((disposition_kind = 'origin_of'::text) AND (delivery_kind = ANY (ARRAY['start_when_no_active_turn'::text, 'after_current_turn'::text, 'interrupt'::text])) AND (((delivery_kind = 'start_when_no_active_turn'::text) AND (expected_active_turn_id IS NULL)) OR ((delivery_kind = ANY (ARRAY['after_current_turn'::text, 'interrupt'::text])) AND (expected_active_turn_id IS NOT NULL))) AND (expected_defaults_version IS NOT NULL) AND (model_override_kind IS NOT NULL) AND (origin_turn_id IS NOT NULL) AND (consuming_model_call_id IS NULL)) OR ((disposition_kind = ANY (ARRAY['pending_steering'::text, 'consumed_as_steering'::text])) AND (delivery_kind = 'next_safe_point'::text) AND (expected_active_turn_id IS NOT NULL) AND (expected_defaults_version IS NULL) AND (model_override_kind IS NULL) AND (replacement_model_kind IS NULL) AND (replacement_direct_model_selection_id IS NULL) AND (replacement_model_alias_id IS NULL) AND (origin_turn_id IS NULL) AND (((disposition_kind = 'pending_steering'::text) AND (consuming_model_call_id IS NULL)) OR ((disposition_kind = 'consumed_as_steering'::text) AND (consuming_model_call_id IS NOT NULL)))) OR ((disposition_kind = 'reclassified_as_turn_origin'::text) AND (delivery_kind = 'next_safe_point'::text) AND (expected_active_turn_id IS NOT NULL) AND (expected_defaults_version IS NULL) AND (model_override_kind IS NULL) AND (replacement_model_kind IS NULL) AND (replacement_direct_model_selection_id IS NULL) AND (replacement_model_alias_id IS NULL) AND (origin_turn_id IS NOT NULL) AND (consuming_model_call_id IS NULL)))),
    CONSTRAINT accepted_input_descendant_scope_shape CHECK ((((delivery_kind = 'interrupt'::text) AND (descendant_scope IS NOT NULL) AND (descendant_scope = ANY (ARRAY['parent_alone'::text, 'parent_and_descendants'::text]))) OR ((delivery_kind <> 'interrupt'::text) AND (descendant_scope IS NULL)))),
    CONSTRAINT accepted_input_disposition_closed CHECK ((disposition_kind = ANY (ARRAY['origin_of'::text, 'pending_steering'::text, 'consumed_as_steering'::text, 'reclassified_as_turn_origin'::text]))),
    CONSTRAINT accepted_input_expected_defaults_positive_u64 CHECK (((expected_defaults_version >= (1)::numeric) AND (expected_defaults_version <= '18446744073709551615'::numeric))),
    CONSTRAINT accepted_input_model_settings_override_object CHECK ((jsonb_typeof(model_settings_override) = 'object'::text)),
    CONSTRAINT accepted_input_position_positive_u64 CHECK (((acceptance_position >= (1)::numeric) AND (acceptance_position <= '18446744073709551615'::numeric)))
);


--
-- Name: accepted_input_content_part; Type: TABLE; Schema: public
--

CREATE TABLE accepted_input_content_part (
    accepted_input_id uuid NOT NULL,
    "position" smallint NOT NULL,
    part_kind text NOT NULL,
    text_value text,
    blob_digest bytea,
    attachment_kind text,
    declared_media_type text,
    display_filename text,
    CONSTRAINT accepted_input_content_part_position CHECK ((("position" >= 0) AND ("position" <= 255))),
    CONSTRAINT accepted_input_content_part_shape CHECK ((((part_kind = 'text'::text) AND (text_value IS NOT NULL) AND (char_length(text_value) > 0) AND (blob_digest IS NULL) AND (attachment_kind IS NULL) AND (declared_media_type IS NULL) AND (display_filename IS NULL)) OR ((part_kind = 'attachment'::text) AND (text_value IS NULL) AND (blob_digest IS NOT NULL) AND (octet_length(blob_digest) = 32) AND (attachment_kind IS NOT NULL) AND (attachment_kind = ANY (ARRAY['image'::text, 'document'::text, 'file'::text])) AND (declared_media_type IS NOT NULL) AND (octet_length(declared_media_type) BETWEEN 1 AND 255) AND ((declared_media_type COLLATE "C") ~ '^[!-~]+$'::text) AND ((display_filename IS NULL) OR ((octet_length(convert_to(display_filename, 'UTF8'::name)) BETWEEN 1 AND 255) AND (display_filename <> ALL (ARRAY['.'::text, '..'::text])) AND (POSITION(('/'::text) IN (display_filename)) = 0) AND (POSITION((chr(92)) IN (display_filename)) = 0))))))
);


--
-- Name: automatic_reconciliation; Type: TABLE; Schema: public
--

CREATE TABLE automatic_reconciliation (
    turn_id uuid CONSTRAINT automatic_model_call_reconciliation_turn_id_not_null NOT NULL,
    session_id uuid CONSTRAINT automatic_model_call_reconciliation_session_id_not_null NOT NULL,
    model_call_id uuid,
    state_kind text DEFAULT 'scheduled'::text CONSTRAINT automatic_model_call_reconciliation_state_kind_not_null NOT NULL,
    attempt_count integer DEFAULT 0 CONSTRAINT automatic_model_call_reconciliation_attempt_count_not_null NOT NULL,
    next_attempt_at timestamp with time zone DEFAULT statement_timestamp() CONSTRAINT automatic_model_call_reconciliation_next_attempt_at_not_null NOT NULL,
    exhausted_at timestamp with time zone,
    tool_attempt_id uuid,
    CONSTRAINT automatic_model_call_reconciliation_attempt_count CHECK (((attempt_count >= 0) AND (attempt_count <= 5))),
    CONSTRAINT automatic_model_call_reconciliation_exhaustion CHECK (((state_kind = 'exhausted'::text) = (exhausted_at IS NOT NULL))),
    CONSTRAINT automatic_model_call_reconciliation_state_kind CHECK ((state_kind = ANY (ARRAY['scheduled'::text, 'attempting'::text, 'reconciled'::text, 'superseded'::text, 'exhausted'::text]))),
    CONSTRAINT automatic_reconciliation_operation CHECK ((num_nonnulls(model_call_id, tool_attempt_id) = 1))
);


--
-- Name: automatic_reconciliation_attempt; Type: TABLE; Schema: public
--

CREATE TABLE automatic_reconciliation_attempt (
    turn_id uuid CONSTRAINT automatic_model_call_reconciliation_attempt_turn_id_not_null NOT NULL,
    attempt_ordinal integer CONSTRAINT automatic_model_call_reconciliation_at_attempt_ordinal_not_null NOT NULL,
    outcome_kind text DEFAULT 'attempting'::text CONSTRAINT automatic_model_call_reconciliation_attem_outcome_kind_not_null NOT NULL,
    started_at timestamp with time zone DEFAULT statement_timestamp() CONSTRAINT automatic_model_call_reconciliation_attempt_started_at_not_null NOT NULL,
    finished_at timestamp with time zone,
    CONSTRAINT automatic_model_call_reconciliation_attempt_finished CHECK (((outcome_kind = 'attempting'::text) = (finished_at IS NULL))),
    CONSTRAINT automatic_model_call_reconciliation_attempt_ordinal CHECK (((attempt_ordinal >= 1) AND (attempt_ordinal <= 5))),
    CONSTRAINT automatic_model_call_reconciliation_attempt_outcome CHECK ((outcome_kind = ANY (ARRAY['attempting'::text, 'reconciled'::text, 'superseded'::text, 'infrastructure_failure'::text, 'integrity_failure'::text])))
);


--
-- Name: automatic_reconciliation_discovery_state; Type: TABLE; Schema: public
--

CREATE TABLE automatic_reconciliation_discovery_state (
    singleton boolean DEFAULT true NOT NULL,
    after_turn_id uuid,
    high_turn_id uuid,
    CONSTRAINT automatic_reconciliation_discovery_state_singleton_check CHECK (singleton)
);


--
-- Name: automatic_reconciliation_supersession_state; Type: TABLE; Schema: public
--

CREATE TABLE automatic_reconciliation_supersession_state (
    singleton boolean DEFAULT true NOT NULL,
    after_turn_id uuid,
    high_turn_id uuid,
    CONSTRAINT automatic_reconciliation_supersession_state_singleton_check CHECK (singleton)
);


--
-- Name: input_accepted_outbox_event; Type: TABLE; Schema: public
--

CREATE TABLE input_accepted_outbox_event (
    event_sequence numeric(20,0) NOT NULL,
    event_kind text NOT NULL,
    storage_version smallint NOT NULL,
    session_id uuid NOT NULL,
    accepted_input_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    acceptance_position numeric(20,0) NOT NULL,
    CONSTRAINT input_accepted_outbox_kind_closed CHECK ((event_kind = 'input_accepted'::text)),
    CONSTRAINT input_accepted_outbox_version_supported CHECK ((storage_version = 1))
);


--
-- Name: queued_input_origin; Type: TABLE; Schema: public
--

CREATE TABLE queued_input_origin (
    turn_id uuid NOT NULL,
    accepted_input_id uuid NOT NULL,
    session_id uuid NOT NULL,
    acceptance_position numeric(20,0) NOT NULL,
    priority_kind text NOT NULL,
    defaults_version numeric(20,0),
    requested_model_kind text,
    requested_direct_model_selection_id uuid,
    requested_model_alias_id uuid,
    frozen_model_kind text,
    frozen_direct_model_selection_id uuid,
    frozen_model_alias_id uuid,
    frozen_alias_selected_direct_id uuid,
    model_parameters text,
    known_provider_failure_retry text,
    model_fallback text,
    source_configuration_turn_id uuid,
    interrupt_predecessor_turn_id uuid,
    dangerous_tool_auto_approval text,
    model_settings_evidence_required boolean DEFAULT true NOT NULL,
    CONSTRAINT queued_input_origin_configuration_provenance_shape CHECK ((((source_configuration_turn_id IS NULL) AND (defaults_version IS NOT NULL) AND (requested_model_kind IS NOT NULL) AND (frozen_model_kind IS NOT NULL) AND (model_parameters IS NOT NULL) AND (known_provider_failure_retry IS NOT NULL) AND (model_fallback IS NOT NULL) AND (dangerous_tool_auto_approval IS NOT NULL)) OR ((source_configuration_turn_id IS NOT NULL) AND (defaults_version IS NULL) AND (requested_model_kind IS NULL) AND (requested_direct_model_selection_id IS NULL) AND (requested_model_alias_id IS NULL) AND (frozen_model_kind IS NULL) AND (frozen_direct_model_selection_id IS NULL) AND (frozen_model_alias_id IS NULL) AND (frozen_alias_selected_direct_id IS NULL) AND (model_parameters IS NULL) AND (known_provider_failure_retry IS NULL) AND (model_fallback IS NULL) AND (dangerous_tool_auto_approval IS NULL)))),
    CONSTRAINT queued_input_origin_defaults_version_positive_u64 CHECK (((defaults_version IS NULL) OR ((defaults_version >= (1)::numeric) AND (defaults_version <= '18446744073709551615'::numeric)))),
    CONSTRAINT queued_input_origin_frozen_model_shape CHECK (((source_configuration_turn_id IS NOT NULL) OR ((frozen_model_kind = 'direct'::text) AND (frozen_direct_model_selection_id IS NOT NULL) AND (frozen_model_alias_id IS NULL) AND (frozen_alias_selected_direct_id IS NULL)) OR ((frozen_model_kind = 'frozen_alias'::text) AND (frozen_direct_model_selection_id IS NULL) AND (frozen_model_alias_id IS NOT NULL) AND (frozen_alias_selected_direct_id IS NOT NULL)))),
    CONSTRAINT queued_input_origin_known_failure_retry_closed CHECK (((source_configuration_turn_id IS NOT NULL) OR (known_provider_failure_retry = 'disabled'::text))),
    CONSTRAINT queued_input_origin_model_fallback_closed CHECK (((source_configuration_turn_id IS NOT NULL) OR (model_fallback = 'disabled'::text))),
    CONSTRAINT queued_input_origin_model_parameters_closed CHECK (((source_configuration_turn_id IS NOT NULL) OR (model_parameters = 'provider_defaults'::text))),
    CONSTRAINT queued_input_origin_position_positive_u64 CHECK (((acceptance_position >= (1)::numeric) AND (acceptance_position <= '18446744073709551615'::numeric))),
    CONSTRAINT queued_input_origin_priority_closed CHECK ((((priority_kind = 'ordinary'::text) AND (interrupt_predecessor_turn_id IS NULL)) OR ((priority_kind = 'interrupt_immediately_after'::text) AND (interrupt_predecessor_turn_id IS NOT NULL)))),
    CONSTRAINT queued_input_origin_requested_model_shape CHECK (((source_configuration_turn_id IS NOT NULL) OR ((requested_model_kind = 'direct'::text) AND (requested_direct_model_selection_id IS NOT NULL) AND (requested_model_alias_id IS NULL)) OR ((requested_model_kind = 'alias'::text) AND (requested_direct_model_selection_id IS NULL) AND (requested_model_alias_id IS NOT NULL)))),
    CONSTRAINT queued_input_origin_source_not_self CHECK ((source_configuration_turn_id IS DISTINCT FROM turn_id)),
    CONSTRAINT queued_input_origin_tool_auto_approval_closed CHECK (((source_configuration_turn_id IS NOT NULL) OR (dangerous_tool_auto_approval = ANY (ARRAY['disabled'::text, 'approve_all'::text]))))
);


--
-- Name: turn_lifecycle; Type: TABLE; Schema: public
--

CREATE TABLE turn_lifecycle (
    turn_id uuid NOT NULL,
    session_id uuid NOT NULL,
    origin_accepted_input_id uuid,
    acceptance_position numeric(20,0) NOT NULL,
    attempt_history_present boolean DEFAULT false NOT NULL,
    state_kind text NOT NULL,
    start_lineage_kind text,
    immediate_predecessor_turn_id uuid,
    starting_frontier_id uuid,
    terminal_frontier_id uuid,
    active_phase_kind text,
    current_attempt_id uuid,
    terminal_disposition_kind text,
    pinned_provider_model_identity_id uuid,
    recovery_model_call_id uuid,
    terminal_attempt_id uuid,
    terminal_model_call_id uuid,
    active_tool_round_call_id uuid,
    approval_tool_request_id uuid,
    recovery_tool_attempt_id uuid,
    terminal_tool_attempt_id uuid,
    model_identity_boundary_required boolean DEFAULT true NOT NULL,
    origin_kind text DEFAULT 'accepted_input'::text NOT NULL,
    delegation_runtime_terminal boolean DEFAULT false NOT NULL,
    child_wait_request_id uuid,
    runner_recovery_runner_id uuid,
    runner_recovery_placement_revision numeric(20,0),
    runner_recovery_tool_attempt_id uuid,
    CONSTRAINT turn_lifecycle_active_phase_closed CHECK (((active_phase_kind IS NULL) OR (active_phase_kind = ANY (ARRAY['running'::text, 'awaiting_model_call_recovery'::text, 'awaiting_tool_approval'::text, 'awaiting_child'::text, 'awaiting_tool_recovery'::text, 'awaiting_runner_recovery'::text])))),
    CONSTRAINT turn_lifecycle_child_wait_shape CHECK ((((active_phase_kind = 'awaiting_child'::text) AND (child_wait_request_id IS NOT NULL)) OR ((active_phase_kind IS DISTINCT FROM 'awaiting_child'::text) AND (child_wait_request_id IS NULL)))),
    CONSTRAINT turn_lifecycle_delegation_runtime_terminal_shape CHECK (((NOT delegation_runtime_terminal) OR (origin_kind = 'delegation'::text))),
    CONSTRAINT turn_lifecycle_lineage_kind_closed CHECK (((start_lineage_kind IS NULL) OR (start_lineage_kind = ANY (ARRAY['first_in_session'::text, 'after'::text])))),
    CONSTRAINT turn_lifecycle_lineage_shape CHECK ((((start_lineage_kind IS NULL) AND (immediate_predecessor_turn_id IS NULL)) OR ((start_lineage_kind = 'first_in_session'::text) AND (immediate_predecessor_turn_id IS NULL)) OR ((start_lineage_kind = 'after'::text) AND (immediate_predecessor_turn_id IS NOT NULL)))),
    CONSTRAINT turn_lifecycle_model_identity_boundary_requirement_state CHECK ((model_identity_boundary_required OR (state_kind = ANY (ARRAY['active'::text, 'terminal'::text])))),
    CONSTRAINT turn_lifecycle_origin_kind_closed CHECK ((((origin_kind = 'accepted_input'::text) AND (origin_accepted_input_id IS NOT NULL)) OR ((origin_kind = 'delegation'::text) AND (origin_accepted_input_id IS NULL)))),
    CONSTRAINT turn_lifecycle_position_positive_u64 CHECK (((acceptance_position >= (1)::numeric) AND (acceptance_position <= '18446744073709551615'::numeric))),
    CONSTRAINT turn_lifecycle_runner_recovery_revision_positive CHECK (((runner_recovery_placement_revision IS NULL) OR ((runner_recovery_placement_revision >= (1)::numeric) AND (runner_recovery_placement_revision <= '18446744073709551615'::numeric)))),
    CONSTRAINT turn_lifecycle_state_kind_closed CHECK ((state_kind = ANY (ARRAY['queued'::text, 'active'::text, 'terminal'::text]))),
    CONSTRAINT turn_lifecycle_state_payload_shape CHECK ((((((state_kind = 'queued'::text) AND (start_lineage_kind IS NULL) AND (immediate_predecessor_turn_id IS NULL) AND (starting_frontier_id IS NULL) AND (terminal_frontier_id IS NULL) AND (active_phase_kind IS NULL) AND (current_attempt_id IS NULL) AND (terminal_disposition_kind IS NULL) AND (recovery_model_call_id IS NULL) AND (active_tool_round_call_id IS NULL) AND (approval_tool_request_id IS NULL) AND (recovery_tool_attempt_id IS NULL) AND (terminal_attempt_id IS NULL) AND (terminal_model_call_id IS NULL) AND (terminal_tool_attempt_id IS NULL)) OR ((state_kind = 'active'::text) AND (start_lineage_kind IS NOT NULL) AND (starting_frontier_id IS NOT NULL) AND (terminal_frontier_id IS NULL) AND (active_phase_kind = 'running'::text) AND (current_attempt_id IS NOT NULL) AND (terminal_disposition_kind IS NULL) AND (recovery_model_call_id IS NULL) AND (approval_tool_request_id IS NULL) AND (recovery_tool_attempt_id IS NULL) AND (terminal_attempt_id IS NULL) AND (terminal_model_call_id IS NULL) AND (terminal_tool_attempt_id IS NULL)) OR ((state_kind = 'active'::text) AND (start_lineage_kind IS NOT NULL) AND (starting_frontier_id IS NOT NULL) AND (terminal_frontier_id IS NULL) AND (active_phase_kind = 'awaiting_model_call_recovery'::text) AND (current_attempt_id IS NOT NULL) AND (terminal_disposition_kind IS NULL) AND (recovery_model_call_id IS NOT NULL) AND (active_tool_round_call_id IS NULL) AND (approval_tool_request_id IS NULL) AND (recovery_tool_attempt_id IS NULL) AND (terminal_attempt_id IS NULL) AND (terminal_model_call_id IS NULL) AND (terminal_tool_attempt_id IS NULL)) OR ((state_kind = 'active'::text) AND (start_lineage_kind IS NOT NULL) AND (starting_frontier_id IS NOT NULL) AND (terminal_frontier_id IS NULL) AND (active_phase_kind = 'awaiting_tool_approval'::text) AND (current_attempt_id IS NULL) AND (terminal_disposition_kind IS NULL) AND (recovery_model_call_id IS NULL) AND (active_tool_round_call_id IS NOT NULL) AND (approval_tool_request_id IS NOT NULL) AND (recovery_tool_attempt_id IS NULL) AND (terminal_attempt_id IS NULL) AND (terminal_model_call_id IS NULL) AND (terminal_tool_attempt_id IS NULL)) OR ((state_kind = 'active'::text) AND (start_lineage_kind IS NOT NULL) AND (starting_frontier_id IS NOT NULL) AND (terminal_frontier_id IS NULL) AND (active_phase_kind = 'awaiting_tool_recovery'::text) AND (current_attempt_id IS NOT NULL) AND (terminal_disposition_kind IS NULL) AND (recovery_model_call_id IS NULL) AND (active_tool_round_call_id IS NOT NULL) AND (approval_tool_request_id IS NULL) AND (recovery_tool_attempt_id IS NOT NULL) AND (terminal_attempt_id IS NULL) AND (terminal_model_call_id IS NULL) AND (terminal_tool_attempt_id IS NULL)) OR ((state_kind = 'terminal'::text) AND (start_lineage_kind IS NOT NULL) AND (starting_frontier_id IS NOT NULL) AND (terminal_frontier_id IS NOT NULL) AND (active_phase_kind IS NULL) AND (current_attempt_id IS NULL) AND (terminal_disposition_kind = 'failed'::text) AND (recovery_model_call_id IS NULL) AND (active_tool_round_call_id IS NULL) AND (approval_tool_request_id IS NULL) AND (recovery_tool_attempt_id IS NULL) AND (((terminal_attempt_id IS NULL) AND (terminal_model_call_id IS NULL) AND (terminal_tool_attempt_id IS NULL)) OR ((terminal_attempt_id IS NOT NULL) AND (terminal_tool_attempt_id IS NULL)))) OR ((state_kind = 'terminal'::text) AND (start_lineage_kind IS NOT NULL) AND (starting_frontier_id IS NOT NULL) AND (terminal_frontier_id IS NOT NULL) AND (active_phase_kind IS NULL) AND (current_attempt_id IS NULL) AND (terminal_disposition_kind = ANY (ARRAY['completed'::text, 'refused'::text])) AND (recovery_model_call_id IS NULL) AND (active_tool_round_call_id IS NULL) AND (approval_tool_request_id IS NULL) AND (recovery_tool_attempt_id IS NULL) AND (terminal_attempt_id IS NOT NULL) AND (terminal_model_call_id IS NOT NULL) AND (terminal_tool_attempt_id IS NULL)) OR ((state_kind = 'terminal'::text) AND (start_lineage_kind IS NOT NULL) AND (starting_frontier_id IS NOT NULL) AND (terminal_frontier_id IS NOT NULL) AND (active_phase_kind IS NULL) AND (current_attempt_id IS NULL) AND (terminal_disposition_kind = 'cancelled'::text) AND (recovery_model_call_id IS NULL) AND (active_tool_round_call_id IS NULL) AND (approval_tool_request_id IS NULL) AND (recovery_tool_attempt_id IS NULL) AND (terminal_attempt_id IS NOT NULL) AND (terminal_tool_attempt_id IS NULL)) OR ((state_kind = 'terminal'::text) AND (start_lineage_kind IS NOT NULL) AND (starting_frontier_id IS NOT NULL) AND (terminal_frontier_id IS NOT NULL) AND (active_phase_kind IS NULL) AND (current_attempt_id IS NULL) AND (terminal_disposition_kind = 'reconciliation_required'::text) AND (recovery_model_call_id IS NULL) AND (active_tool_round_call_id IS NULL) AND (approval_tool_request_id IS NULL) AND (recovery_tool_attempt_id IS NULL) AND (terminal_attempt_id IS NOT NULL) AND (((terminal_model_call_id IS NOT NULL) AND (terminal_tool_attempt_id IS NULL)) OR ((terminal_model_call_id IS NULL) AND (terminal_tool_attempt_id IS NOT NULL)))) OR ((state_kind = 'active'::text) AND (start_lineage_kind IS NOT NULL) AND (starting_frontier_id IS NOT NULL) AND (terminal_frontier_id IS NULL) AND (active_phase_kind = 'awaiting_child'::text) AND (current_attempt_id IS NULL) AND (terminal_disposition_kind IS NULL) AND (recovery_model_call_id IS NULL) AND (active_tool_round_call_id IS NOT NULL) AND (approval_tool_request_id IS NULL) AND (recovery_tool_attempt_id IS NULL) AND (terminal_attempt_id IS NULL) AND (terminal_model_call_id IS NULL) AND (terminal_tool_attempt_id IS NULL)) OR ((state_kind = 'terminal'::text) AND (start_lineage_kind IS NOT NULL) AND (starting_frontier_id IS NOT NULL) AND (terminal_frontier_id IS NOT NULL) AND (active_phase_kind IS NULL) AND (current_attempt_id IS NULL) AND (terminal_disposition_kind = 'cancelled'::text) AND (recovery_model_call_id IS NULL) AND (active_tool_round_call_id IS NULL) AND (approval_tool_request_id IS NULL) AND (recovery_tool_attempt_id IS NULL) AND (terminal_attempt_id IS NULL) AND (terminal_model_call_id IS NULL) AND (terminal_tool_attempt_id IS NULL))) AND (runner_recovery_runner_id IS NULL) AND (runner_recovery_placement_revision IS NULL) AND (runner_recovery_tool_attempt_id IS NULL)) OR ((state_kind = 'active'::text) AND (start_lineage_kind IS NOT NULL) AND (starting_frontier_id IS NOT NULL) AND (terminal_frontier_id IS NULL) AND (active_phase_kind = 'awaiting_runner_recovery'::text) AND (current_attempt_id IS NULL) AND (terminal_disposition_kind IS NULL) AND (recovery_model_call_id IS NULL) AND (approval_tool_request_id IS NULL) AND (recovery_tool_attempt_id IS NULL) AND (child_wait_request_id IS NULL) AND (terminal_attempt_id IS NULL) AND (terminal_model_call_id IS NULL) AND (terminal_tool_attempt_id IS NULL) AND (runner_recovery_runner_id IS NOT NULL) AND (runner_recovery_placement_revision IS NOT NULL)))),
    CONSTRAINT turn_lifecycle_terminal_disposition_closed CHECK (((terminal_disposition_kind IS NULL) OR (terminal_disposition_kind = ANY (ARRAY['failed'::text, 'completed'::text, 'refused'::text, 'cancelled'::text, 'reconciliation_required'::text]))))
);


--
-- Name: semantic_transcript_entry; Type: TABLE; Schema: public
--

CREATE TABLE semantic_transcript_entry (
    source_session_id uuid NOT NULL,
    semantic_entry_id uuid NOT NULL,
    payload_kind text NOT NULL,
    origin_accepted_input_id uuid,
    failed_turn_id uuid,
    assistant_text_value text,
    producing_model_call_id uuid,
    assistant_tool_request_id uuid,
    completed_turn_id uuid,
    steering_source_turn_id uuid,
    cancelled_turn_id uuid,
    imported_conversation_id uuid,
    imported_transcript_entry_id uuid,
    tool_result_request_id uuid,
    tool_result_attempt_id uuid,
    assistant_response_part_ordinal numeric(10,0),
    model_identity_turn_id uuid,
    model_identity_defaults_version numeric(20,0),
    model_identity_direct_selection_id uuid,
    context_summary_value text,
    context_summary_producing_call_id uuid,
    context_summary_first_source_session_id uuid,
    context_summary_first_entry_id uuid,
    context_summary_through_source_session_id uuid,
    context_summary_through_entry_id uuid,
    delegated_task_spawning_tool_request_id uuid,
    delegation_message_id uuid,
    delegation_result_awaiting_tool_request_id uuid,
    delegation_result_spawning_tool_request_id uuid,
    assistant_response_text_start_bytes numeric(20,0),
    CONSTRAINT semantic_transcript_entry_imported_shape CHECK ((((payload_kind = 'imported_entry'::text) AND (imported_conversation_id IS NOT NULL) AND (imported_transcript_entry_id IS NOT NULL) AND (origin_accepted_input_id IS NULL) AND (steering_source_turn_id IS NULL) AND (failed_turn_id IS NULL) AND (assistant_text_value IS NULL) AND (producing_model_call_id IS NULL) AND (assistant_tool_request_id IS NULL) AND (completed_turn_id IS NULL) AND (cancelled_turn_id IS NULL)) OR ((payload_kind <> 'imported_entry'::text) AND (imported_conversation_id IS NULL) AND (imported_transcript_entry_id IS NULL)))),
    CONSTRAINT semantic_transcript_entry_model_identity_version_positive_u64 CHECK (((model_identity_defaults_version IS NULL) OR ((model_identity_defaults_version >= (1)::numeric) AND (model_identity_defaults_version <= '18446744073709551615'::numeric)))),
    CONSTRAINT semantic_transcript_entry_payload_kind_closed CHECK (((payload_kind = ANY (ARRAY['delegation_message'::text, 'delegation_result'::text])) OR ((payload_kind = 'delegated_task'::text) OR (payload_kind = ANY (ARRAY['imported_entry'::text, 'origin_accepted_input'::text, 'steering_accepted_input'::text, 'model_identity_changed'::text, 'context_summary'::text, 'turn_failed'::text, 'assistant_text'::text, 'assistant_tool_use'::text, 'tool_execution_result'::text, 'tool_denied'::text, 'tool_closed_by_turn_end'::text, 'turn_completed'::text, 'turn_cancelled'::text]))))),
    CONSTRAINT semantic_transcript_entry_payload_shape CHECK ((((payload_kind = 'delegation_message'::text) AND (delegation_message_id IS NOT NULL) AND (tool_result_request_id IS NULL) AND (delegation_result_awaiting_tool_request_id IS NULL) AND (delegation_result_spawning_tool_request_id IS NULL) AND (origin_accepted_input_id IS NULL) AND (failed_turn_id IS NULL) AND (assistant_text_value IS NULL) AND (producing_model_call_id IS NULL) AND (assistant_tool_request_id IS NULL) AND (completed_turn_id IS NULL) AND (steering_source_turn_id IS NULL) AND (cancelled_turn_id IS NULL) AND (imported_conversation_id IS NULL) AND (imported_transcript_entry_id IS NULL) AND (tool_result_attempt_id IS NULL) AND (assistant_response_part_ordinal IS NULL) AND (model_identity_turn_id IS NULL) AND (model_identity_defaults_version IS NULL) AND (model_identity_direct_selection_id IS NULL) AND (context_summary_value IS NULL) AND (context_summary_producing_call_id IS NULL) AND (context_summary_first_source_session_id IS NULL) AND (context_summary_first_entry_id IS NULL) AND (context_summary_through_source_session_id IS NULL) AND (context_summary_through_entry_id IS NULL) AND (delegated_task_spawning_tool_request_id IS NULL)) OR ((payload_kind = 'delegation_result'::text) AND (delegation_message_id IS NULL) AND (delegation_result_awaiting_tool_request_id IS NOT NULL) AND ((tool_result_request_id IS NULL) OR (tool_result_request_id = delegation_result_awaiting_tool_request_id)) AND (delegation_result_spawning_tool_request_id IS NOT NULL) AND (origin_accepted_input_id IS NULL) AND (failed_turn_id IS NULL) AND (assistant_text_value IS NULL) AND (producing_model_call_id IS NULL) AND (assistant_tool_request_id IS NULL) AND (completed_turn_id IS NULL) AND (steering_source_turn_id IS NULL) AND (cancelled_turn_id IS NULL) AND (imported_conversation_id IS NULL) AND (imported_transcript_entry_id IS NULL) AND (tool_result_attempt_id IS NULL) AND (assistant_response_part_ordinal IS NULL) AND (model_identity_turn_id IS NULL) AND (model_identity_defaults_version IS NULL) AND (model_identity_direct_selection_id IS NULL) AND (context_summary_value IS NULL) AND (context_summary_producing_call_id IS NULL) AND (context_summary_first_source_session_id IS NULL) AND (context_summary_first_entry_id IS NULL) AND (context_summary_through_source_session_id IS NULL) AND (context_summary_through_entry_id IS NULL) AND (delegated_task_spawning_tool_request_id IS NULL)) OR ((payload_kind <> ALL (ARRAY['delegation_message'::text, 'delegation_result'::text])) AND (delegation_message_id IS NULL) AND (delegation_result_awaiting_tool_request_id IS NULL) AND (delegation_result_spawning_tool_request_id IS NULL) AND (((payload_kind = 'delegated_task'::text) AND (delegated_task_spawning_tool_request_id IS NOT NULL) AND (origin_accepted_input_id IS NULL) AND (failed_turn_id IS NULL) AND (assistant_text_value IS NULL) AND (producing_model_call_id IS NULL) AND (assistant_tool_request_id IS NULL) AND (completed_turn_id IS NULL) AND (steering_source_turn_id IS NULL) AND (cancelled_turn_id IS NULL) AND (imported_conversation_id IS NULL) AND (imported_transcript_entry_id IS NULL) AND (tool_result_request_id IS NULL) AND (tool_result_attempt_id IS NULL) AND (assistant_response_part_ordinal IS NULL) AND (model_identity_turn_id IS NULL) AND (model_identity_defaults_version IS NULL) AND (model_identity_direct_selection_id IS NULL) AND (context_summary_value IS NULL) AND (context_summary_producing_call_id IS NULL) AND (context_summary_first_source_session_id IS NULL) AND (context_summary_first_entry_id IS NULL) AND (context_summary_through_source_session_id IS NULL) AND (context_summary_through_entry_id IS NULL)) OR ((payload_kind <> 'delegated_task'::text) AND (delegated_task_spawning_tool_request_id IS NULL) AND (((payload_kind = 'context_summary'::text) AND (context_summary_value IS NOT NULL) AND (context_summary_value <> ''::text) AND (context_summary_producing_call_id IS NOT NULL) AND (context_summary_first_source_session_id IS NOT NULL) AND (context_summary_first_entry_id IS NOT NULL) AND (context_summary_through_source_session_id IS NOT NULL) AND (context_summary_through_entry_id IS NOT NULL) AND (origin_accepted_input_id IS NULL) AND (steering_source_turn_id IS NULL) AND (model_identity_turn_id IS NULL) AND (model_identity_defaults_version IS NULL) AND (model_identity_direct_selection_id IS NULL) AND (failed_turn_id IS NULL) AND (cancelled_turn_id IS NULL) AND (assistant_text_value IS NULL) AND (producing_model_call_id IS NULL) AND (assistant_tool_request_id IS NULL) AND (tool_result_request_id IS NULL) AND (tool_result_attempt_id IS NULL) AND (completed_turn_id IS NULL) AND (imported_conversation_id IS NULL) AND (imported_transcript_entry_id IS NULL) AND (assistant_response_part_ordinal IS NULL)) OR ((payload_kind <> 'context_summary'::text) AND (context_summary_value IS NULL) AND (context_summary_producing_call_id IS NULL) AND (context_summary_first_source_session_id IS NULL) AND (context_summary_first_entry_id IS NULL) AND (context_summary_through_source_session_id IS NULL) AND (context_summary_through_entry_id IS NULL) AND (((payload_kind = 'model_identity_changed'::text) AND (model_identity_turn_id IS NOT NULL) AND (model_identity_defaults_version IS NOT NULL) AND (model_identity_direct_selection_id IS NOT NULL) AND (origin_accepted_input_id IS NULL) AND (steering_source_turn_id IS NULL) AND (failed_turn_id IS NULL) AND (cancelled_turn_id IS NULL) AND (assistant_text_value IS NULL) AND (producing_model_call_id IS NULL) AND (assistant_tool_request_id IS NULL) AND (tool_result_request_id IS NULL) AND (tool_result_attempt_id IS NULL) AND (completed_turn_id IS NULL) AND (imported_conversation_id IS NULL) AND (imported_transcript_entry_id IS NULL) AND (assistant_response_part_ordinal IS NULL)) OR ((payload_kind <> 'model_identity_changed'::text) AND (model_identity_turn_id IS NULL) AND (model_identity_defaults_version IS NULL) AND (model_identity_direct_selection_id IS NULL) AND (((payload_kind = 'imported_entry'::text) AND (tool_result_request_id IS NULL) AND (tool_result_attempt_id IS NULL) AND (assistant_response_part_ordinal IS NULL)) OR ((payload_kind = ANY (ARRAY['origin_accepted_input'::text, 'steering_accepted_input'::text])) AND (origin_accepted_input_id IS NOT NULL) AND (((payload_kind = 'origin_accepted_input'::text) AND (steering_source_turn_id IS NULL)) OR ((payload_kind = 'steering_accepted_input'::text) AND (steering_source_turn_id IS NOT NULL))) AND (failed_turn_id IS NULL) AND (assistant_text_value IS NULL) AND (producing_model_call_id IS NULL) AND (assistant_tool_request_id IS NULL) AND (tool_result_request_id IS NULL) AND (tool_result_attempt_id IS NULL) AND (completed_turn_id IS NULL) AND (cancelled_turn_id IS NULL)) OR ((payload_kind = 'turn_failed'::text) AND (origin_accepted_input_id IS NULL) AND (steering_source_turn_id IS NULL) AND (failed_turn_id IS NOT NULL) AND (assistant_text_value IS NULL) AND (producing_model_call_id IS NULL) AND (assistant_tool_request_id IS NULL) AND (tool_result_request_id IS NULL) AND (tool_result_attempt_id IS NULL) AND (completed_turn_id IS NULL) AND (cancelled_turn_id IS NULL)) OR ((payload_kind = 'assistant_text'::text) AND (origin_accepted_input_id IS NULL) AND (steering_source_turn_id IS NULL) AND (failed_turn_id IS NULL) AND (assistant_text_value IS NOT NULL) AND (assistant_text_value <> ''::text) AND (producing_model_call_id IS NOT NULL) AND (assistant_tool_request_id IS NULL) AND (tool_result_request_id IS NULL) AND (tool_result_attempt_id IS NULL) AND (completed_turn_id IS NULL) AND (cancelled_turn_id IS NULL)) OR ((payload_kind = 'assistant_tool_use'::text) AND (origin_accepted_input_id IS NULL) AND (steering_source_turn_id IS NULL) AND (failed_turn_id IS NULL) AND (assistant_text_value IS NULL) AND (producing_model_call_id IS NOT NULL) AND (assistant_tool_request_id IS NOT NULL) AND (tool_result_request_id IS NULL) AND (tool_result_attempt_id IS NULL) AND (completed_turn_id IS NULL) AND (cancelled_turn_id IS NULL)) OR ((payload_kind = 'tool_execution_result'::text) AND (origin_accepted_input_id IS NULL) AND (steering_source_turn_id IS NULL) AND (failed_turn_id IS NULL) AND (assistant_text_value IS NULL) AND (producing_model_call_id IS NULL) AND (assistant_tool_request_id IS NULL) AND (tool_result_request_id IS NULL) AND (tool_result_attempt_id IS NOT NULL) AND (completed_turn_id IS NULL) AND (cancelled_turn_id IS NULL)) OR ((payload_kind = ANY (ARRAY['tool_denied'::text, 'tool_closed_by_turn_end'::text])) AND (origin_accepted_input_id IS NULL) AND (steering_source_turn_id IS NULL) AND (failed_turn_id IS NULL) AND (assistant_text_value IS NULL) AND (producing_model_call_id IS NULL) AND (assistant_tool_request_id IS NULL) AND (tool_result_request_id IS NOT NULL) AND (tool_result_attempt_id IS NULL) AND (completed_turn_id IS NULL) AND (cancelled_turn_id IS NULL)) OR ((payload_kind = 'turn_completed'::text) AND (origin_accepted_input_id IS NULL) AND (steering_source_turn_id IS NULL) AND (failed_turn_id IS NULL) AND (assistant_text_value IS NULL) AND (producing_model_call_id IS NULL) AND (assistant_tool_request_id IS NULL) AND (tool_result_request_id IS NULL) AND (tool_result_attempt_id IS NULL) AND (completed_turn_id IS NOT NULL) AND (cancelled_turn_id IS NULL)) OR ((payload_kind = 'turn_cancelled'::text) AND (origin_accepted_input_id IS NULL) AND (steering_source_turn_id IS NULL) AND (failed_turn_id IS NULL) AND (assistant_text_value IS NULL) AND (producing_model_call_id IS NULL) AND (assistant_tool_request_id IS NULL) AND (tool_result_request_id IS NULL) AND (tool_result_attempt_id IS NULL) AND (completed_turn_id IS NULL) AND (cancelled_turn_id IS NOT NULL)))))))))))),
    CONSTRAINT semantic_transcript_entry_response_part_ordinal_shape CHECK (((assistant_response_part_ordinal IS NULL) OR ((payload_kind = ANY (ARRAY['assistant_text'::text, 'assistant_tool_use'::text])) AND (producing_model_call_id IS NOT NULL) AND ((assistant_response_part_ordinal >= (0)::numeric) AND (assistant_response_part_ordinal <= ('4294967295'::bigint)::numeric))))),
    CONSTRAINT semantic_transcript_entry_response_text_position_shape CHECK ((((payload_kind = 'assistant_text'::text) AND (assistant_response_part_ordinal IS NOT NULL) AND (assistant_response_text_start_bytes IS NOT NULL) AND ((assistant_response_text_start_bytes >= (0)::numeric) AND (assistant_response_text_start_bytes <= '18446744073709551615'::numeric))) OR ((payload_kind <> 'assistant_text'::text) AND (assistant_response_text_start_bytes IS NULL))))
);


--
-- Name: submit_input_command; Type: TABLE; Schema: public
--

CREATE TABLE submit_input_command (
    command_id uuid NOT NULL,
    command_kind text NOT NULL,
    storage_version smallint NOT NULL,
    session_id uuid NOT NULL,
    actor_kind text NOT NULL,
    actor_turn_id uuid,
    actor_tool_request_id uuid,
    delivery_kind text NOT NULL,
    expected_active_turn_id uuid,
    expected_defaults_version numeric(20,0),
    model_override_kind text,
    replacement_model_kind text,
    replacement_direct_model_selection_id uuid,
    replacement_model_alias_id uuid,
    result_kind text NOT NULL,
    rejection_kind text,
    result_session_id uuid NOT NULL,
    result_accepted_input_id uuid,
    result_turn_id uuid,
    result_expected_active_turn_id uuid,
    result_expected_defaults_version numeric(20,0),
    result_current_defaults_version numeric(20,0),
    result_unknown_alias_id uuid,
    result_selected_defaults_version numeric(20,0),
    result_last_position numeric(20,0),
    result_actual_active_turn_id uuid,
    result_existing_interrupt_command_id uuid,
    descendant_scope text,
    model_settings_override jsonb DEFAULT '{"fast_mode": {"kind": "inherit"}, "service_tier": {"kind": "inherit"}, "reasoning_level": {"kind": "inherit"}}'::jsonb NOT NULL,
    content_parts_creation_xid xid8 DEFAULT pg_current_xact_id() NOT NULL,
    result_attachment_digest bytea,
    result_attachment_maximum_bytes numeric(20,0),
    CONSTRAINT submit_input_command_actor_kind_closed CHECK ((actor_kind = ANY (ARRAY['user'::text, 'model'::text, 'recovery'::text, 'tool'::text]))),
    CONSTRAINT submit_input_command_actor_shape CHECK ((((actor_kind = ANY (ARRAY['user'::text, 'recovery'::text])) AND (actor_turn_id IS NULL) AND (actor_tool_request_id IS NULL)) OR ((actor_kind = 'model'::text) AND (actor_turn_id IS NOT NULL) AND (actor_tool_request_id IS NULL)) OR ((actor_kind = 'tool'::text) AND (actor_turn_id IS NULL) AND (actor_tool_request_id IS NOT NULL)))),
    CONSTRAINT submit_input_command_attachment_result_evidence_shape CHECK (((((rejection_kind = 'attachment_blob_not_found'::text) AND (result_kind = 'rejected'::text) AND (result_accepted_input_id IS NULL) AND (result_turn_id IS NULL) AND (result_actual_active_turn_id IS NULL) AND (result_expected_active_turn_id IS NULL) AND (result_expected_defaults_version IS NULL) AND (result_current_defaults_version IS NULL) AND (result_unknown_alias_id IS NULL) AND (result_selected_defaults_version IS NULL) AND (result_last_position IS NULL) AND (result_existing_interrupt_command_id IS NULL) AND (result_attachment_digest IS NOT NULL) AND (result_attachment_maximum_bytes IS NULL)) OR ((rejection_kind = 'attachment_byte_budget_exceeded'::text) AND (result_kind = 'rejected'::text) AND (result_accepted_input_id IS NULL) AND (result_turn_id IS NULL) AND (result_actual_active_turn_id IS NULL) AND (result_expected_active_turn_id IS NULL) AND (result_expected_defaults_version IS NULL) AND (result_current_defaults_version IS NULL) AND (result_unknown_alias_id IS NULL) AND (result_selected_defaults_version IS NULL) AND (result_last_position IS NULL) AND (result_existing_interrupt_command_id IS NULL) AND (result_attachment_digest IS NULL) AND (result_attachment_maximum_bytes IS NOT NULL)) OR (((rejection_kind IS NULL) OR (rejection_kind <> ALL (ARRAY['attachment_blob_not_found'::text, 'attachment_byte_budget_exceeded'::text]))) AND (result_attachment_digest IS NULL) AND (result_attachment_maximum_bytes IS NULL))) IS TRUE)),
    CONSTRAINT submit_input_command_configuration_shape CHECK ((((model_override_kind IS NULL) AND (replacement_model_kind IS NULL) AND (replacement_direct_model_selection_id IS NULL) AND (replacement_model_alias_id IS NULL)) OR ((model_override_kind = 'use_session_default'::text) AND (replacement_model_kind IS NULL) AND (replacement_direct_model_selection_id IS NULL) AND (replacement_model_alias_id IS NULL)) OR ((model_override_kind = 'replace_with'::text) AND (replacement_model_kind = 'direct'::text) AND (replacement_direct_model_selection_id IS NOT NULL) AND (replacement_model_alias_id IS NULL)) OR ((model_override_kind = 'replace_with'::text) AND (replacement_model_kind = 'alias'::text) AND (replacement_direct_model_selection_id IS NULL) AND (replacement_model_alias_id IS NOT NULL)))),
    CONSTRAINT submit_input_command_delivery_kind_closed CHECK ((delivery_kind = ANY (ARRAY['start_when_no_active_turn'::text, 'interrupt'::text, 'next_safe_point'::text, 'after_current_turn'::text]))),
    CONSTRAINT submit_input_command_delivery_shape CHECK ((((delivery_kind = 'start_when_no_active_turn'::text) AND (expected_active_turn_id IS NULL) AND (expected_defaults_version IS NOT NULL) AND (model_override_kind IS NOT NULL)) OR ((delivery_kind = ANY (ARRAY['interrupt'::text, 'after_current_turn'::text])) AND (expected_active_turn_id IS NOT NULL) AND (expected_defaults_version IS NOT NULL) AND (model_override_kind IS NOT NULL)) OR ((delivery_kind = 'next_safe_point'::text) AND (expected_active_turn_id IS NOT NULL) AND (expected_defaults_version IS NULL) AND (model_override_kind IS NULL)))),
    CONSTRAINT submit_input_command_descendant_scope_shape CHECK ((((delivery_kind = 'interrupt'::text) AND (descendant_scope IS NOT NULL) AND (descendant_scope = ANY (ARRAY['parent_alone'::text, 'parent_and_descendants'::text]))) OR ((delivery_kind <> 'interrupt'::text) AND (descendant_scope IS NULL)))),
    CONSTRAINT submit_input_command_expected_defaults_positive_u64 CHECK (((expected_defaults_version IS NULL) OR ((expected_defaults_version >= (1)::numeric) AND (expected_defaults_version <= '18446744073709551615'::numeric)))),
    CONSTRAINT submit_input_command_kind_closed CHECK ((command_kind = 'submit_input'::text)),
    CONSTRAINT submit_input_command_model_settings_override_object CHECK ((jsonb_typeof(model_settings_override) = 'object'::text)),
    CONSTRAINT submit_input_command_rejection_kind_closed CHECK (((rejection_kind IS NULL) OR (rejection_kind = ANY (ARRAY['attachment_blob_not_found'::text, 'attachment_byte_budget_exceeded'::text, 'session_not_found'::text, 'no_active_turn'::text, 'active_turn_present'::text, 'active_turn_mismatch'::text, 'session_defaults_version_mismatch'::text, 'unknown_model_alias'::text, 'acceptance_position_exhausted'::text, 'safe_point_unavailable_while_stopping'::text, 'interrupt_already_applied'::text, 'interrupt_unavailable_while_awaiting_approval'::text])))),
    CONSTRAINT submit_input_command_result_attachment_digest_shape CHECK (((result_attachment_digest IS NULL) OR (octet_length(result_attachment_digest) = 32))),
    CONSTRAINT submit_input_command_result_attachment_maximum_bytes_u64 CHECK (((result_attachment_maximum_bytes IS NULL) OR ((result_attachment_maximum_bytes >= (1)::numeric) AND (result_attachment_maximum_bytes <= '18446744073709551615'::numeric)))),
    CONSTRAINT submit_input_command_result_current_defaults_positive_u64 CHECK (((result_current_defaults_version IS NULL) OR ((result_current_defaults_version >= (1)::numeric) AND (result_current_defaults_version <= '18446744073709551615'::numeric)))),
    CONSTRAINT submit_input_command_result_expected_defaults_positive_u64 CHECK (((result_expected_defaults_version IS NULL) OR ((result_expected_defaults_version >= (1)::numeric) AND (result_expected_defaults_version <= '18446744073709551615'::numeric)))),
    CONSTRAINT submit_input_command_result_kind_closed CHECK ((result_kind = ANY (ARRAY['applied'::text, 'rejected'::text]))),
    CONSTRAINT submit_input_command_result_last_position_positive_u64 CHECK (((result_last_position IS NULL) OR ((result_last_position >= (1)::numeric) AND (result_last_position <= '18446744073709551615'::numeric)))),
    CONSTRAINT submit_input_command_result_selected_defaults_positive_u64 CHECK (((result_selected_defaults_version IS NULL) OR ((result_selected_defaults_version >= (1)::numeric) AND (result_selected_defaults_version <= '18446744073709551615'::numeric)))),
    CONSTRAINT submit_input_command_result_session_matches CHECK ((result_session_id = session_id)),
    CONSTRAINT submit_input_command_result_shape CHECK ((((result_kind = 'applied'::text) AND (rejection_kind IS NULL) AND (delivery_kind = ANY (ARRAY['start_when_no_active_turn'::text, 'after_current_turn'::text, 'interrupt'::text])) AND (result_accepted_input_id IS NOT NULL) AND (result_turn_id IS NOT NULL) AND (result_actual_active_turn_id IS NULL) AND (result_expected_active_turn_id IS NULL) AND (result_expected_defaults_version IS NULL) AND (result_current_defaults_version IS NULL) AND (result_unknown_alias_id IS NULL) AND (result_selected_defaults_version IS NULL) AND (result_last_position IS NULL) AND (result_existing_interrupt_command_id IS NULL)) OR ((result_kind = 'applied'::text) AND (rejection_kind IS NULL) AND (delivery_kind = 'next_safe_point'::text) AND (result_accepted_input_id IS NOT NULL) AND (result_turn_id IS NULL) AND (result_actual_active_turn_id = expected_active_turn_id) AND (result_actual_active_turn_id IS NOT NULL) AND (result_expected_active_turn_id IS NULL) AND (result_expected_defaults_version IS NULL) AND (result_current_defaults_version IS NULL) AND (result_unknown_alias_id IS NULL) AND (result_selected_defaults_version IS NULL) AND (result_last_position IS NULL) AND (result_existing_interrupt_command_id IS NULL)) OR ((result_kind = 'rejected'::text) AND (rejection_kind = 'session_not_found'::text) AND (result_accepted_input_id IS NULL) AND (result_turn_id IS NULL) AND (result_actual_active_turn_id IS NULL) AND (result_expected_active_turn_id IS NULL) AND (result_expected_defaults_version IS NULL) AND (result_current_defaults_version IS NULL) AND (result_unknown_alias_id IS NULL) AND (result_selected_defaults_version IS NULL) AND (result_last_position IS NULL) AND (result_existing_interrupt_command_id IS NULL)) OR ((result_kind = 'rejected'::text) AND (rejection_kind = 'no_active_turn'::text) AND (delivery_kind = ANY (ARRAY['interrupt'::text, 'next_safe_point'::text, 'after_current_turn'::text])) AND (result_accepted_input_id IS NULL) AND (result_turn_id IS NULL) AND (result_actual_active_turn_id IS NULL) AND (result_expected_active_turn_id = expected_active_turn_id) AND (result_expected_active_turn_id IS NOT NULL) AND (result_expected_defaults_version IS NULL) AND (result_current_defaults_version IS NULL) AND (result_unknown_alias_id IS NULL) AND (result_selected_defaults_version IS NULL) AND (result_last_position IS NULL) AND (result_existing_interrupt_command_id IS NULL)) OR ((result_kind = 'rejected'::text) AND (rejection_kind = 'active_turn_present'::text) AND (delivery_kind = 'start_when_no_active_turn'::text) AND (result_accepted_input_id IS NULL) AND (result_turn_id IS NULL) AND (result_actual_active_turn_id IS NOT NULL) AND (result_expected_active_turn_id IS NULL) AND (result_expected_defaults_version IS NULL) AND (result_current_defaults_version IS NULL) AND (result_unknown_alias_id IS NULL) AND (result_selected_defaults_version IS NULL) AND (result_last_position IS NULL) AND (result_existing_interrupt_command_id IS NULL)) OR ((result_kind = 'rejected'::text) AND (rejection_kind = 'active_turn_mismatch'::text) AND (delivery_kind = ANY (ARRAY['interrupt'::text, 'next_safe_point'::text, 'after_current_turn'::text])) AND (result_accepted_input_id IS NULL) AND (result_turn_id IS NULL) AND (result_actual_active_turn_id IS NOT NULL) AND (result_expected_active_turn_id = expected_active_turn_id) AND (result_expected_active_turn_id IS NOT NULL) AND (result_actual_active_turn_id <> result_expected_active_turn_id) AND (result_expected_defaults_version IS NULL) AND (result_current_defaults_version IS NULL) AND (result_unknown_alias_id IS NULL) AND (result_selected_defaults_version IS NULL) AND (result_last_position IS NULL) AND (result_existing_interrupt_command_id IS NULL)) OR ((result_kind = 'rejected'::text) AND (rejection_kind = 'session_defaults_version_mismatch'::text) AND (delivery_kind = ANY (ARRAY['start_when_no_active_turn'::text, 'after_current_turn'::text, 'interrupt'::text])) AND (result_accepted_input_id IS NULL) AND (result_turn_id IS NULL) AND (result_actual_active_turn_id IS NULL) AND (result_expected_active_turn_id IS NULL) AND (result_expected_defaults_version = expected_defaults_version) AND (result_expected_defaults_version IS NOT NULL) AND (result_current_defaults_version IS NOT NULL) AND (result_current_defaults_version <> result_expected_defaults_version) AND (result_unknown_alias_id IS NULL) AND (result_selected_defaults_version IS NULL) AND (result_last_position IS NULL) AND (result_existing_interrupt_command_id IS NULL)) OR ((result_kind = 'rejected'::text) AND (rejection_kind = 'unknown_model_alias'::text) AND (delivery_kind = ANY (ARRAY['start_when_no_active_turn'::text, 'after_current_turn'::text, 'interrupt'::text])) AND (result_accepted_input_id IS NULL) AND (result_turn_id IS NULL) AND (result_actual_active_turn_id IS NULL) AND (result_expected_active_turn_id IS NULL) AND (result_expected_defaults_version IS NULL) AND (result_current_defaults_version IS NULL) AND (result_unknown_alias_id IS NOT NULL) AND (result_selected_defaults_version = expected_defaults_version) AND (result_selected_defaults_version IS NOT NULL) AND (result_last_position IS NULL) AND (result_existing_interrupt_command_id IS NULL)) OR ((result_kind = 'rejected'::text) AND (rejection_kind = 'acceptance_position_exhausted'::text) AND (delivery_kind = ANY (ARRAY['start_when_no_active_turn'::text, 'next_safe_point'::text, 'after_current_turn'::text, 'interrupt'::text])) AND (result_accepted_input_id IS NULL) AND (result_turn_id IS NULL) AND (result_actual_active_turn_id IS NULL) AND (result_expected_active_turn_id IS NULL) AND (result_expected_defaults_version IS NULL) AND (result_current_defaults_version IS NULL) AND (result_unknown_alias_id IS NULL) AND (result_selected_defaults_version IS NULL) AND (result_last_position IS NOT NULL) AND (result_last_position = '18446744073709551615'::numeric) AND (result_existing_interrupt_command_id IS NULL)) OR ((result_kind = 'rejected'::text) AND (((rejection_kind = 'safe_point_unavailable_while_stopping'::text) AND (delivery_kind = 'next_safe_point'::text)) OR ((rejection_kind = 'interrupt_already_applied'::text) AND (delivery_kind = 'interrupt'::text))) AND (result_accepted_input_id IS NULL) AND (result_turn_id IS NULL) AND (result_actual_active_turn_id = expected_active_turn_id) AND (result_actual_active_turn_id IS NOT NULL) AND (result_expected_active_turn_id IS NULL) AND (result_expected_defaults_version IS NULL) AND (result_current_defaults_version IS NULL) AND (result_unknown_alias_id IS NULL) AND (result_selected_defaults_version IS NULL) AND (result_last_position IS NULL) AND (result_existing_interrupt_command_id IS NOT NULL)) OR ((result_kind = 'rejected'::text) AND (rejection_kind = 'interrupt_unavailable_while_awaiting_approval'::text) AND (delivery_kind = 'interrupt'::text) AND (result_accepted_input_id IS NULL) AND (result_turn_id IS NULL) AND (result_actual_active_turn_id = expected_active_turn_id) AND (result_actual_active_turn_id IS NOT NULL) AND (result_expected_active_turn_id IS NULL) AND (result_expected_defaults_version IS NULL) AND (result_current_defaults_version IS NULL) AND (result_unknown_alias_id IS NULL) AND (result_selected_defaults_version IS NULL) AND (result_last_position IS NULL) AND (result_existing_interrupt_command_id IS NULL)) OR ((result_kind = 'rejected'::text) AND (rejection_kind = 'attachment_blob_not_found'::text) AND (result_accepted_input_id IS NULL) AND (result_turn_id IS NULL) AND (result_actual_active_turn_id IS NULL) AND (result_expected_active_turn_id IS NULL) AND (result_expected_defaults_version IS NULL) AND (result_current_defaults_version IS NULL) AND (result_unknown_alias_id IS NULL) AND (result_selected_defaults_version IS NULL) AND (result_last_position IS NULL) AND (result_existing_interrupt_command_id IS NULL) AND (result_attachment_digest IS NOT NULL) AND (result_attachment_maximum_bytes IS NULL)) OR ((result_kind = 'rejected'::text) AND (rejection_kind = 'attachment_byte_budget_exceeded'::text) AND (result_accepted_input_id IS NULL) AND (result_turn_id IS NULL) AND (result_actual_active_turn_id IS NULL) AND (result_expected_active_turn_id IS NULL) AND (result_expected_defaults_version IS NULL) AND (result_current_defaults_version IS NULL) AND (result_unknown_alias_id IS NULL) AND (result_selected_defaults_version IS NULL) AND (result_last_position IS NULL) AND (result_existing_interrupt_command_id IS NULL) AND (result_attachment_digest IS NULL) AND (result_attachment_maximum_bytes IS NOT NULL)))),
    CONSTRAINT submit_input_command_storage_version_supported CHECK ((storage_version = 3))
);


--
-- Name: submit_input_command_content_part; Type: TABLE; Schema: public
--

CREATE TABLE submit_input_command_content_part (
    command_id uuid NOT NULL,
    "position" smallint NOT NULL,
    part_kind text NOT NULL,
    text_value text,
    blob_digest bytea,
    attachment_kind text,
    declared_media_type text,
    display_filename text,
    CONSTRAINT submit_input_command_content_part_position CHECK ((("position" >= 0) AND ("position" <= 255))),
    CONSTRAINT submit_input_command_content_part_shape CHECK ((((part_kind = 'text'::text) AND (text_value IS NOT NULL) AND (char_length(text_value) > 0) AND (blob_digest IS NULL) AND (attachment_kind IS NULL) AND (declared_media_type IS NULL) AND (display_filename IS NULL)) OR ((part_kind = 'attachment'::text) AND (text_value IS NULL) AND (blob_digest IS NOT NULL) AND (octet_length(blob_digest) = 32) AND (attachment_kind IS NOT NULL) AND (attachment_kind = ANY (ARRAY['image'::text, 'document'::text, 'file'::text])) AND (declared_media_type IS NOT NULL) AND (octet_length(declared_media_type) BETWEEN 1 AND 255) AND ((declared_media_type COLLATE "C") ~ '^[!-~]+$'::text) AND ((display_filename IS NULL) OR ((octet_length(convert_to(display_filename, 'UTF8'::name)) BETWEEN 1 AND 255) AND (display_filename <> ALL (ARRAY['.'::text, '..'::text])) AND (POSITION(('/'::text) IN (display_filename)) = 0) AND (POSITION((chr(92)) IN (display_filename)) = 0))))))
);


--
-- Name: turn_activated_outbox_event; Type: TABLE; Schema: public
--

CREATE TABLE turn_activated_outbox_event (
    event_sequence numeric(20,0) NOT NULL,
    event_kind text NOT NULL,
    storage_version smallint NOT NULL,
    session_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    current_attempt_id uuid NOT NULL,
    CONSTRAINT turn_activated_outbox_kind_closed CHECK ((event_kind = 'turn_activated'::text)),
    CONSTRAINT turn_activated_outbox_version_supported CHECK ((storage_version = 1))
);


--
-- Name: turn_attempt; Type: TABLE; Schema: public
--

CREATE TABLE turn_attempt (
    turn_attempt_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    session_id uuid NOT NULL,
    continued_from_attempt_id uuid,
    state_kind text NOT NULL,
    end_variant text,
    end_disposition text,
    interrupt_command_id uuid,
    interrupt_predecessor_turn_id uuid,
    CONSTRAINT turn_attempt_end_disposition_closed CHECK (((end_disposition IS NULL) OR (end_disposition = ANY (ARRAY['turn_completed'::text, 'turn_refused'::text, 'yielded_to_durable_wait'::text, 'known_failure'::text, 'lost'::text, 'cancelled'::text, 'ambiguous'::text])))),
    CONSTRAINT turn_attempt_end_variant_closed CHECK (((end_variant IS NULL) OR (end_variant = ANY (ARRAY['without_stop'::text, 'after_cancellation'::text])))),
    CONSTRAINT turn_attempt_state_kind_closed CHECK ((state_kind = ANY (ARRAY['prepared'::text, 'running'::text, 'stop_requested'::text, 'ended'::text]))),
    CONSTRAINT turn_attempt_state_payload_shape CHECK ((((state_kind = ANY (ARRAY['prepared'::text, 'running'::text])) AND (end_variant IS NULL) AND (end_disposition IS NULL) AND (interrupt_command_id IS NULL) AND (interrupt_predecessor_turn_id IS NULL)) OR ((state_kind = 'stop_requested'::text) AND (end_variant IS NULL) AND (end_disposition IS NULL) AND (interrupt_command_id IS NOT NULL) AND (interrupt_predecessor_turn_id = turn_id)) OR ((state_kind = 'ended'::text) AND (end_variant = 'without_stop'::text) AND (end_disposition IS NOT NULL) AND (interrupt_command_id IS NULL) AND (interrupt_predecessor_turn_id IS NULL)) OR ((state_kind = 'ended'::text) AND (end_variant = 'after_cancellation'::text) AND (end_disposition = ANY (ARRAY['turn_completed'::text, 'turn_refused'::text, 'known_failure'::text, 'lost'::text, 'cancelled'::text, 'ambiguous'::text])) AND (interrupt_command_id IS NOT NULL) AND (interrupt_predecessor_turn_id = turn_id))))
);


--
-- Name: turn_cancelled_outbox_event; Type: TABLE; Schema: public
--

CREATE TABLE turn_cancelled_outbox_event (
    event_sequence numeric(20,0) NOT NULL,
    event_kind text NOT NULL,
    storage_version smallint NOT NULL,
    session_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    cancellation_entry_id uuid NOT NULL,
    terminal_frontier_id uuid NOT NULL,
    CONSTRAINT turn_cancelled_outbox_kind_closed CHECK ((event_kind = 'turn_cancelled'::text)),
    CONSTRAINT turn_cancelled_outbox_version_supported CHECK ((storage_version = 1))
);


--
-- Name: turn_completed_outbox_event; Type: TABLE; Schema: public
--

CREATE TABLE turn_completed_outbox_event (
    event_sequence numeric(20,0) NOT NULL,
    event_kind text NOT NULL,
    storage_version smallint NOT NULL,
    session_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    model_call_id uuid NOT NULL,
    completion_entry_id uuid NOT NULL,
    terminal_frontier_id uuid NOT NULL,
    CONSTRAINT turn_completed_outbox_kind_closed CHECK ((event_kind = 'turn_completed'::text)),
    CONSTRAINT turn_completed_outbox_version_supported CHECK ((storage_version = 1))
);


--
-- Name: turn_failed_outbox_event; Type: TABLE; Schema: public
--

CREATE TABLE turn_failed_outbox_event (
    event_sequence numeric(20,0) NOT NULL,
    event_kind text NOT NULL,
    storage_version smallint NOT NULL,
    session_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    failure_entry_id uuid NOT NULL,
    terminal_frontier_id uuid NOT NULL,
    CONSTRAINT turn_failed_outbox_event_kind_closed CHECK ((event_kind = 'turn_failed'::text)),
    CONSTRAINT turn_failed_outbox_event_storage_version_supported CHECK ((storage_version = 1))
);


--
-- Name: turn_model_settings_resolved; Type: TABLE; Schema: public
--

CREATE TABLE turn_model_settings_resolved (
    accepted_input_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    session_id uuid NOT NULL,
    defaults_version numeric(20,0) NOT NULL,
    selected_direct_model_id uuid NOT NULL,
    per_call_model_settings jsonb NOT NULL,
    resolved_model_settings jsonb NOT NULL,
    adjusted_from_selection_id uuid,
    adjustments jsonb NOT NULL,
    CONSTRAINT turn_model_settings_resolved_adjustment_source CHECK ((((adjusted_from_selection_id IS NULL) AND (adjustments = '[]'::jsonb)) OR ((adjusted_from_selection_id IS NOT NULL) AND (adjusted_from_selection_id <> selected_direct_model_id) AND (jsonb_array_length(adjustments) > 0)))),
    CONSTRAINT turn_model_settings_resolved_documents CHECK (((jsonb_typeof(per_call_model_settings) = 'object'::text) AND (jsonb_typeof(resolved_model_settings) = 'object'::text) AND (jsonb_typeof(adjustments) = 'array'::text)))
);


--
-- Name: turn_model_settings_resolved_outbox_event; Type: TABLE; Schema: public
--

CREATE TABLE turn_model_settings_resolved_outbox_event (
    event_sequence numeric(20,0) CONSTRAINT turn_model_settings_resolved_outbox_eve_event_sequence_not_null NOT NULL,
    event_kind text NOT NULL,
    storage_version smallint CONSTRAINT turn_model_settings_resolved_outbox_ev_storage_version_not_null NOT NULL,
    session_id uuid NOT NULL,
    accepted_input_id uuid CONSTRAINT turn_model_settings_resolved_outbox__accepted_input_id_not_null NOT NULL,
    CONSTRAINT turn_model_settings_resolved_outbox_kind_closed CHECK ((event_kind = 'turn_model_settings_resolved'::text)),
    CONSTRAINT turn_model_settings_resolved_outbox_version_supported CHECK ((storage_version = 1))
);


--
-- Name: turn_reconciliation_required_outbox_event; Type: TABLE; Schema: public
--

CREATE TABLE turn_reconciliation_required_outbox_event (
    event_sequence numeric(20,0) CONSTRAINT turn_reconciliation_required_outbox_eve_event_sequence_not_null NOT NULL,
    event_kind text NOT NULL,
    storage_version smallint CONSTRAINT turn_reconciliation_required_outbox_ev_storage_version_not_null NOT NULL,
    session_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    model_call_id uuid,
    terminal_frontier_id uuid CONSTRAINT turn_reconciliation_required_outb_terminal_frontier_id_not_null NOT NULL,
    tool_attempt_id uuid,
    CONSTRAINT turn_reconciliation_required_outbox_kind_closed CHECK ((event_kind = 'turn_reconciliation_required'::text)),
    CONSTRAINT turn_reconciliation_required_outbox_operation_shape CHECK (((((model_call_id IS NOT NULL))::integer + ((tool_attempt_id IS NOT NULL))::integer) = 1)),
    CONSTRAINT turn_reconciliation_required_outbox_version_supported CHECK ((storage_version = 1))
);


--
-- Name: turn_refused_outbox_event; Type: TABLE; Schema: public
--

CREATE TABLE turn_refused_outbox_event (
    event_sequence numeric(20,0) NOT NULL,
    event_kind text NOT NULL,
    storage_version smallint NOT NULL,
    session_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    model_call_id uuid NOT NULL,
    terminal_frontier_id uuid NOT NULL,
    CONSTRAINT turn_refused_outbox_kind_closed CHECK ((event_kind = 'turn_refused'::text)),
    CONSTRAINT turn_refused_outbox_version_supported CHECK ((storage_version = 1))
);


--
-- Name: turn_restart_recovery_origin; Type: TABLE; Schema: public
--

CREATE TABLE turn_restart_recovery_origin (
    turn_id uuid NOT NULL,
    session_id uuid NOT NULL,
    recorded_at timestamp with time zone DEFAULT transaction_timestamp() NOT NULL
);


--
-- Name: turn_runner_recovery_interrupt_effect; Type: TABLE; Schema: public
--

CREATE TABLE turn_runner_recovery_interrupt_effect (
    command_id uuid NOT NULL,
    session_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    placement_event_ordinal numeric(20,0) CONSTRAINT turn_runner_recovery_interrupt_placement_event_ordinal_not_null NOT NULL,
    runner_id uuid NOT NULL,
    placement_revision numeric(20,0) CONSTRAINT turn_runner_recovery_interrupt_effe_placement_revision_not_null NOT NULL,
    yielded_turn_attempt_id uuid CONSTRAINT turn_runner_recovery_interrupt_yielded_turn_attempt_id_not_null NOT NULL,
    interrupted_tool_attempt_id uuid,
    source_frontier_id uuid CONSTRAINT turn_runner_recovery_interrupt_effe_source_frontier_id_not_null NOT NULL,
    CONSTRAINT turn_runner_recovery_interrupt_ef_placement_event_ordinal_check CHECK (((placement_event_ordinal >= (1)::numeric) AND (placement_event_ordinal <= '18446744073709551615'::numeric))),
    CONSTRAINT turn_runner_recovery_interrupt_effect_placement_revision_check CHECK (((placement_revision >= (1)::numeric) AND (placement_revision <= '18446744073709551615'::numeric)))
);


--
-- Constraints.
--

--
-- Name: accepted_input accepted_input_accepting_command_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY accepted_input
    ADD CONSTRAINT accepted_input_accepting_command_id_key UNIQUE (accepting_command_id);


--
-- Name: accepted_input_content_part accepted_input_content_part_pk; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY accepted_input_content_part
    ADD CONSTRAINT accepted_input_content_part_pk PRIMARY KEY (accepted_input_id, "position");


--
-- Name: accepted_input accepted_input_effect_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY accepted_input
    ADD CONSTRAINT accepted_input_effect_key UNIQUE (accepted_input_id, session_id, acceptance_position, origin_turn_id);


--
-- Name: accepted_input accepted_input_general_command_result_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY accepted_input
    ADD CONSTRAINT accepted_input_general_command_result_key UNIQUE (accepting_command_id, accepted_input_id, session_id);


--
-- Name: accepted_input accepted_input_origin_turn_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY accepted_input
    ADD CONSTRAINT accepted_input_origin_turn_id_key UNIQUE (origin_turn_id);


--
-- Name: accepted_input accepted_input_pending_result_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY accepted_input
    ADD CONSTRAINT accepted_input_pending_result_key UNIQUE (accepted_input_id, session_id, expected_active_turn_id);


--
-- Name: accepted_input accepted_input_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY accepted_input
    ADD CONSTRAINT accepted_input_pkey PRIMARY KEY (accepted_input_id);


--
-- Name: accepted_input accepted_input_result_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY accepted_input
    ADD CONSTRAINT accepted_input_result_key UNIQUE (accepted_input_id, session_id, origin_turn_id);


--
-- Name: accepted_input accepted_input_session_position_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY accepted_input
    ADD CONSTRAINT accepted_input_session_position_key UNIQUE (session_id, acceptance_position);


--
-- Name: accepted_input accepted_input_source_session_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY accepted_input
    ADD CONSTRAINT accepted_input_source_session_key UNIQUE (accepted_input_id, session_id);


--
-- Name: automatic_reconciliation_attempt automatic_model_call_reconciliation_attempt_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY automatic_reconciliation_attempt
    ADD CONSTRAINT automatic_model_call_reconciliation_attempt_pkey PRIMARY KEY (turn_id, attempt_ordinal);


--
-- Name: automatic_reconciliation automatic_model_call_reconciliation_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY automatic_reconciliation
    ADD CONSTRAINT automatic_model_call_reconciliation_pkey PRIMARY KEY (turn_id);


--
-- Name: automatic_reconciliation_discovery_state automatic_reconciliation_discovery_state_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY automatic_reconciliation_discovery_state
    ADD CONSTRAINT automatic_reconciliation_discovery_state_pkey PRIMARY KEY (singleton);


--
-- Name: automatic_reconciliation_supersession_state automatic_reconciliation_supersession_state_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY automatic_reconciliation_supersession_state
    ADD CONSTRAINT automatic_reconciliation_supersession_state_pkey PRIMARY KEY (singleton);


--
-- Name: input_accepted_outbox_event input_accepted_outbox_event_accepted_input_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY input_accepted_outbox_event
    ADD CONSTRAINT input_accepted_outbox_event_accepted_input_id_key UNIQUE (accepted_input_id);


--
-- Name: input_accepted_outbox_event input_accepted_outbox_event_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY input_accepted_outbox_event
    ADD CONSTRAINT input_accepted_outbox_event_pkey PRIMARY KEY (event_sequence);


--
-- Name: input_accepted_outbox_event input_accepted_outbox_event_turn_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY input_accepted_outbox_event
    ADD CONSTRAINT input_accepted_outbox_event_turn_id_key UNIQUE (turn_id);


--
-- Name: queued_input_origin queued_input_origin_accepted_input_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY queued_input_origin
    ADD CONSTRAINT queued_input_origin_accepted_input_id_key UNIQUE (accepted_input_id);


--
-- Name: queued_input_origin queued_input_origin_effect_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY queued_input_origin
    ADD CONSTRAINT queued_input_origin_effect_key UNIQUE (accepted_input_id, session_id, acceptance_position, turn_id);


--
-- Name: queued_input_origin queued_input_origin_interrupt_edge_once; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY queued_input_origin
    ADD CONSTRAINT queued_input_origin_interrupt_edge_once UNIQUE (interrupt_predecessor_turn_id);


--
-- Name: queued_input_origin queued_input_origin_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY queued_input_origin
    ADD CONSTRAINT queued_input_origin_pkey PRIMARY KEY (turn_id);


--
-- Name: queued_input_origin queued_input_origin_turn_session_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY queued_input_origin
    ADD CONSTRAINT queued_input_origin_turn_session_key UNIQUE (turn_id, session_id);


--
-- Name: semantic_transcript_entry semantic_transcript_entry_delegated_task_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY semantic_transcript_entry
    ADD CONSTRAINT semantic_transcript_entry_delegated_task_key UNIQUE (delegated_task_spawning_tool_request_id, source_session_id, semantic_entry_id);


--
-- Name: semantic_transcript_entry semantic_transcript_entry_delegation_message_once; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY semantic_transcript_entry
    ADD CONSTRAINT semantic_transcript_entry_delegation_message_once UNIQUE (delegation_message_id);


--
-- Name: semantic_transcript_entry semantic_transcript_entry_delegation_result_await_once; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY semantic_transcript_entry
    ADD CONSTRAINT semantic_transcript_entry_delegation_result_await_once UNIQUE (delegation_result_awaiting_tool_request_id);


--
-- Name: semantic_transcript_entry semantic_transcript_entry_id_global; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY semantic_transcript_entry
    ADD CONSTRAINT semantic_transcript_entry_id_global UNIQUE (semantic_entry_id);


--
-- Name: semantic_transcript_entry semantic_transcript_entry_imported_entry_once_per_session; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY semantic_transcript_entry
    ADD CONSTRAINT semantic_transcript_entry_imported_entry_once_per_session UNIQUE (source_session_id, imported_conversation_id, imported_transcript_entry_id) DEFERRABLE INITIALLY DEFERRED;


--
-- Name: semantic_transcript_entry semantic_transcript_entry_model_identity_turn_once; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY semantic_transcript_entry
    ADD CONSTRAINT semantic_transcript_entry_model_identity_turn_once UNIQUE (model_identity_turn_id);


--
-- Name: semantic_transcript_entry semantic_transcript_entry_origin_once; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY semantic_transcript_entry
    ADD CONSTRAINT semantic_transcript_entry_origin_once UNIQUE (origin_accepted_input_id);


--
-- Name: semantic_transcript_entry semantic_transcript_entry_pk; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY semantic_transcript_entry
    ADD CONSTRAINT semantic_transcript_entry_pk PRIMARY KEY (source_session_id, semantic_entry_id);


--
-- Name: semantic_transcript_entry semantic_transcript_entry_response_part_ordinal_once; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY semantic_transcript_entry
    ADD CONSTRAINT semantic_transcript_entry_response_part_ordinal_once UNIQUE (producing_model_call_id, assistant_response_part_ordinal);


--
-- Name: semantic_transcript_entry semantic_transcript_entry_tool_result_attempt_once; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY semantic_transcript_entry
    ADD CONSTRAINT semantic_transcript_entry_tool_result_attempt_once UNIQUE (tool_result_attempt_id);


--
-- Name: semantic_transcript_entry semantic_transcript_entry_tool_result_request_once; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY semantic_transcript_entry
    ADD CONSTRAINT semantic_transcript_entry_tool_result_request_once UNIQUE (tool_result_request_id);


--
-- Name: semantic_transcript_entry semantic_transcript_entry_turn_cancelled_once; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY semantic_transcript_entry
    ADD CONSTRAINT semantic_transcript_entry_turn_cancelled_once UNIQUE (cancelled_turn_id);


--
-- Name: semantic_transcript_entry semantic_transcript_entry_turn_completed_once; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY semantic_transcript_entry
    ADD CONSTRAINT semantic_transcript_entry_turn_completed_once UNIQUE (completed_turn_id);


--
-- Name: semantic_transcript_entry semantic_transcript_entry_turn_failed_once; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY semantic_transcript_entry
    ADD CONSTRAINT semantic_transcript_entry_turn_failed_once UNIQUE (failed_turn_id);


--
-- Name: submit_input_command submit_input_command_accepted_result_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY submit_input_command
    ADD CONSTRAINT submit_input_command_accepted_result_key UNIQUE (command_id, result_accepted_input_id, result_session_id);


--
-- Name: submit_input_command_content_part submit_input_command_content_part_pk; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY submit_input_command_content_part
    ADD CONSTRAINT submit_input_command_content_part_pk PRIMARY KEY (command_id, "position");


--
-- Name: submit_input_command submit_input_command_general_applied_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY submit_input_command
    ADD CONSTRAINT submit_input_command_general_applied_key UNIQUE (command_id, result_accepted_input_id, result_session_id);


--
-- Name: submit_input_command submit_input_command_pending_correlation_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY submit_input_command
    ADD CONSTRAINT submit_input_command_pending_correlation_key UNIQUE (command_id, result_accepted_input_id, result_session_id, result_actual_active_turn_id);


--
-- Name: submit_input_command submit_input_command_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY submit_input_command
    ADD CONSTRAINT submit_input_command_pkey PRIMARY KEY (command_id);


--
-- Name: submit_input_command submit_input_command_result_correlation_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY submit_input_command
    ADD CONSTRAINT submit_input_command_result_correlation_key UNIQUE (command_id, result_accepted_input_id, result_session_id, result_turn_id);


--
-- Name: submit_input_command submit_input_command_scope_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY submit_input_command
    ADD CONSTRAINT submit_input_command_scope_key UNIQUE (command_id, descendant_scope);


--
-- Name: submit_input_command submit_input_command_settings_result_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY submit_input_command
    ADD CONSTRAINT submit_input_command_settings_result_key UNIQUE (command_id, result_accepted_input_id, result_session_id, model_settings_override);


--
-- Name: turn_activated_outbox_event turn_activated_outbox_event_current_attempt_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_activated_outbox_event
    ADD CONSTRAINT turn_activated_outbox_event_current_attempt_id_key UNIQUE (current_attempt_id);


--
-- Name: turn_activated_outbox_event turn_activated_outbox_event_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_activated_outbox_event
    ADD CONSTRAINT turn_activated_outbox_event_pkey PRIMARY KEY (event_sequence);


--
-- Name: turn_activated_outbox_event turn_activated_outbox_event_turn_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_activated_outbox_event
    ADD CONSTRAINT turn_activated_outbox_event_turn_id_key UNIQUE (turn_id);


--
-- Name: turn_attempt turn_attempt_interrupt_command_once; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_attempt
    ADD CONSTRAINT turn_attempt_interrupt_command_once UNIQUE (interrupt_command_id);


--
-- Name: turn_attempt turn_attempt_one_successor_per_predecessor; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_attempt
    ADD CONSTRAINT turn_attempt_one_successor_per_predecessor UNIQUE (continued_from_attempt_id);


--
-- Name: turn_attempt turn_attempt_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_attempt
    ADD CONSTRAINT turn_attempt_pkey PRIMARY KEY (turn_attempt_id);


--
-- Name: turn_attempt turn_attempt_turn_correlation_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_attempt
    ADD CONSTRAINT turn_attempt_turn_correlation_key UNIQUE (turn_attempt_id, turn_id, session_id);


--
-- Name: turn_cancelled_outbox_event turn_cancelled_outbox_event_cancellation_entry_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_cancelled_outbox_event
    ADD CONSTRAINT turn_cancelled_outbox_event_cancellation_entry_id_key UNIQUE (cancellation_entry_id);


--
-- Name: turn_cancelled_outbox_event turn_cancelled_outbox_event_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_cancelled_outbox_event
    ADD CONSTRAINT turn_cancelled_outbox_event_pkey PRIMARY KEY (event_sequence);


--
-- Name: turn_cancelled_outbox_event turn_cancelled_outbox_event_terminal_frontier_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_cancelled_outbox_event
    ADD CONSTRAINT turn_cancelled_outbox_event_terminal_frontier_id_key UNIQUE (terminal_frontier_id);


--
-- Name: turn_cancelled_outbox_event turn_cancelled_outbox_event_turn_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_cancelled_outbox_event
    ADD CONSTRAINT turn_cancelled_outbox_event_turn_id_key UNIQUE (turn_id);


--
-- Name: turn_completed_outbox_event turn_completed_outbox_event_completion_entry_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_completed_outbox_event
    ADD CONSTRAINT turn_completed_outbox_event_completion_entry_id_key UNIQUE (completion_entry_id);


--
-- Name: turn_completed_outbox_event turn_completed_outbox_event_model_call_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_completed_outbox_event
    ADD CONSTRAINT turn_completed_outbox_event_model_call_id_key UNIQUE (model_call_id);


--
-- Name: turn_completed_outbox_event turn_completed_outbox_event_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_completed_outbox_event
    ADD CONSTRAINT turn_completed_outbox_event_pkey PRIMARY KEY (event_sequence);


--
-- Name: turn_completed_outbox_event turn_completed_outbox_event_terminal_frontier_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_completed_outbox_event
    ADD CONSTRAINT turn_completed_outbox_event_terminal_frontier_id_key UNIQUE (terminal_frontier_id);


--
-- Name: turn_completed_outbox_event turn_completed_outbox_event_turn_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_completed_outbox_event
    ADD CONSTRAINT turn_completed_outbox_event_turn_id_key UNIQUE (turn_id);


--
-- Name: turn_failed_outbox_event turn_failed_outbox_event_failure_entry_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_failed_outbox_event
    ADD CONSTRAINT turn_failed_outbox_event_failure_entry_id_key UNIQUE (failure_entry_id);


--
-- Name: turn_failed_outbox_event turn_failed_outbox_event_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_failed_outbox_event
    ADD CONSTRAINT turn_failed_outbox_event_pkey PRIMARY KEY (event_sequence);


--
-- Name: turn_failed_outbox_event turn_failed_outbox_event_terminal_frontier_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_failed_outbox_event
    ADD CONSTRAINT turn_failed_outbox_event_terminal_frontier_id_key UNIQUE (terminal_frontier_id);


--
-- Name: turn_failed_outbox_event turn_failed_outbox_event_turn_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_failed_outbox_event
    ADD CONSTRAINT turn_failed_outbox_event_turn_id_key UNIQUE (turn_id);


--
-- Name: turn_lifecycle turn_lifecycle_delegation_origin_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_lifecycle
    ADD CONSTRAINT turn_lifecycle_delegation_origin_key UNIQUE (turn_id, session_id, acceptance_position);


--
-- Name: turn_lifecycle turn_lifecycle_origin_accepted_input_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_lifecycle
    ADD CONSTRAINT turn_lifecycle_origin_accepted_input_id_key UNIQUE (origin_accepted_input_id);


--
-- Name: turn_lifecycle turn_lifecycle_origin_correlation_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_lifecycle
    ADD CONSTRAINT turn_lifecycle_origin_correlation_key UNIQUE (origin_accepted_input_id, session_id, acceptance_position, turn_id);


--
-- Name: turn_lifecycle turn_lifecycle_pinned_target_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_lifecycle
    ADD CONSTRAINT turn_lifecycle_pinned_target_key UNIQUE (turn_id, session_id, pinned_provider_model_identity_id);


--
-- Name: turn_lifecycle turn_lifecycle_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_lifecycle
    ADD CONSTRAINT turn_lifecycle_pkey PRIMARY KEY (turn_id);


--
-- Name: turn_lifecycle turn_lifecycle_review_pass_origin_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_lifecycle
    ADD CONSTRAINT turn_lifecycle_review_pass_origin_key UNIQUE (turn_id, session_id, origin_accepted_input_id);


--
-- Name: turn_lifecycle turn_lifecycle_review_pass_terminal_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_lifecycle
    ADD CONSTRAINT turn_lifecycle_review_pass_terminal_key UNIQUE (turn_id, session_id, origin_accepted_input_id, terminal_frontier_id);


--
-- Name: turn_lifecycle turn_lifecycle_turn_session_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_lifecycle
    ADD CONSTRAINT turn_lifecycle_turn_session_key UNIQUE (turn_id, session_id);


--
-- Name: turn_model_settings_resolved_outbox_event turn_model_settings_resolved_outbox_event_accepted_input_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_model_settings_resolved_outbox_event
    ADD CONSTRAINT turn_model_settings_resolved_outbox_event_accepted_input_id_key UNIQUE (accepted_input_id);


--
-- Name: turn_model_settings_resolved_outbox_event turn_model_settings_resolved_outbox_event_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_model_settings_resolved_outbox_event
    ADD CONSTRAINT turn_model_settings_resolved_outbox_event_pkey PRIMARY KEY (event_sequence);


--
-- Name: turn_model_settings_resolved turn_model_settings_resolved_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_model_settings_resolved
    ADD CONSTRAINT turn_model_settings_resolved_pkey PRIMARY KEY (accepted_input_id);


--
-- Name: turn_model_settings_resolved turn_model_settings_resolved_session_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_model_settings_resolved
    ADD CONSTRAINT turn_model_settings_resolved_session_key UNIQUE (accepted_input_id, session_id);


--
-- Name: turn_model_settings_resolved turn_model_settings_resolved_turn_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_model_settings_resolved
    ADD CONSTRAINT turn_model_settings_resolved_turn_id_key UNIQUE (turn_id);


--
-- Name: turn_reconciliation_required_outbox_event turn_reconciliation_required_outbox_ev_terminal_frontier_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_reconciliation_required_outbox_event
    ADD CONSTRAINT turn_reconciliation_required_outbox_ev_terminal_frontier_id_key UNIQUE (terminal_frontier_id);


--
-- Name: turn_reconciliation_required_outbox_event turn_reconciliation_required_outbox_event_model_call_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_reconciliation_required_outbox_event
    ADD CONSTRAINT turn_reconciliation_required_outbox_event_model_call_id_key UNIQUE (model_call_id);


--
-- Name: turn_reconciliation_required_outbox_event turn_reconciliation_required_outbox_event_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_reconciliation_required_outbox_event
    ADD CONSTRAINT turn_reconciliation_required_outbox_event_pkey PRIMARY KEY (event_sequence);


--
-- Name: turn_reconciliation_required_outbox_event turn_reconciliation_required_outbox_event_tool_attempt_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_reconciliation_required_outbox_event
    ADD CONSTRAINT turn_reconciliation_required_outbox_event_tool_attempt_id_key UNIQUE (tool_attempt_id);


--
-- Name: turn_reconciliation_required_outbox_event turn_reconciliation_required_outbox_event_turn_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_reconciliation_required_outbox_event
    ADD CONSTRAINT turn_reconciliation_required_outbox_event_turn_id_key UNIQUE (turn_id);


--
-- Name: turn_refused_outbox_event turn_refused_outbox_event_model_call_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_refused_outbox_event
    ADD CONSTRAINT turn_refused_outbox_event_model_call_id_key UNIQUE (model_call_id);


--
-- Name: turn_refused_outbox_event turn_refused_outbox_event_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_refused_outbox_event
    ADD CONSTRAINT turn_refused_outbox_event_pkey PRIMARY KEY (event_sequence);


--
-- Name: turn_refused_outbox_event turn_refused_outbox_event_terminal_frontier_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_refused_outbox_event
    ADD CONSTRAINT turn_refused_outbox_event_terminal_frontier_id_key UNIQUE (terminal_frontier_id);


--
-- Name: turn_refused_outbox_event turn_refused_outbox_event_turn_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_refused_outbox_event
    ADD CONSTRAINT turn_refused_outbox_event_turn_id_key UNIQUE (turn_id);


--
-- Name: turn_restart_recovery_origin turn_restart_recovery_origin_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_restart_recovery_origin
    ADD CONSTRAINT turn_restart_recovery_origin_pkey PRIMARY KEY (turn_id);


--
-- Name: turn_restart_recovery_origin turn_restart_recovery_origin_turn_id_session_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_restart_recovery_origin
    ADD CONSTRAINT turn_restart_recovery_origin_turn_id_session_id_key UNIQUE (turn_id, session_id);


--
-- Name: turn_runner_recovery_interrupt_effect turn_runner_recovery_interrupt_effect_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_runner_recovery_interrupt_effect
    ADD CONSTRAINT turn_runner_recovery_interrupt_effect_pkey PRIMARY KEY (command_id);


--
-- Name: turn_runner_recovery_interrupt_effect turn_runner_recovery_interrupt_effect_session_id_turn_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_runner_recovery_interrupt_effect
    ADD CONSTRAINT turn_runner_recovery_interrupt_effect_session_id_turn_id_key UNIQUE (session_id, turn_id);


--
-- Indexes.
--

--
-- Name: accepted_input_consumed_by_model_call; Type: INDEX; Schema: public
--

CREATE INDEX accepted_input_consumed_by_model_call ON accepted_input USING btree (session_id, consuming_model_call_id, acceptance_position) WHERE (disposition_kind = 'consumed_as_steering'::text);


--
-- Name: accepted_input_pending_by_source_turn; Type: INDEX; Schema: public
--

CREATE INDEX accepted_input_pending_by_source_turn ON accepted_input USING btree (session_id, expected_active_turn_id) WHERE (disposition_kind = 'pending_steering'::text);


--
-- Name: automatic_reconciliation_due; Type: INDEX; Schema: public
--

CREATE INDEX automatic_reconciliation_due ON automatic_reconciliation USING btree (next_attempt_at, turn_id) WHERE (state_kind = ANY (ARRAY['scheduled'::text, 'attempting'::text]));


--
-- Name: automatic_reconciliation_supersession; Type: INDEX; Schema: public
--

CREATE INDEX automatic_reconciliation_supersession ON automatic_reconciliation USING btree (turn_id) INCLUDE (session_id, model_call_id, tool_attempt_id, state_kind, attempt_count) WHERE (state_kind = ANY (ARRAY['scheduled'::text, 'attempting'::text, 'exhausted'::text]));


--
-- Name: queued_input_origin_by_session_position; Type: INDEX; Schema: public
--

CREATE INDEX queued_input_origin_by_session_position ON queued_input_origin USING btree (session_id, acceptance_position);


--
-- Name: semantic_transcript_response_text_position_once; Type: INDEX; Schema: public
--

CREATE UNIQUE INDEX semantic_transcript_response_text_position_once ON semantic_transcript_entry USING btree (producing_model_call_id, assistant_response_text_start_bytes) WHERE (payload_kind = 'assistant_text'::text);


--
-- Name: turn_attempt_by_turn_session; Type: INDEX; Schema: public
--

CREATE INDEX turn_attempt_by_turn_session ON turn_attempt USING btree (turn_id, session_id);


--
-- Name: turn_attempt_one_initial_per_turn; Type: INDEX; Schema: public
--

CREATE UNIQUE INDEX turn_attempt_one_initial_per_turn ON turn_attempt USING btree (turn_id) WHERE (continued_from_attempt_id IS NULL);


--
-- Name: turn_attempt_one_live_per_turn; Type: INDEX; Schema: public
--

CREATE UNIQUE INDEX turn_attempt_one_live_per_turn ON turn_attempt USING btree (turn_id) WHERE (state_kind <> 'ended'::text);


--
-- Name: turn_lifecycle_automatic_reconciliation_discovery; Type: INDEX; Schema: public
--

CREATE INDEX turn_lifecycle_automatic_reconciliation_discovery ON turn_lifecycle USING btree (turn_id) INCLUDE (session_id, recovery_model_call_id, recovery_tool_attempt_id) WHERE ((state_kind = 'active'::text) AND (active_phase_kind = ANY (ARRAY['awaiting_model_call_recovery'::text, 'awaiting_tool_recovery'::text])) AND (NOT delegation_runtime_terminal) AND (num_nonnulls(recovery_model_call_id, recovery_tool_attempt_id) = 1));


--
-- Name: turn_lifecycle_by_session_position; Type: INDEX; Schema: public
--

CREATE INDEX turn_lifecycle_by_session_position ON turn_lifecycle USING btree (session_id, acceptance_position);


--
-- Name: turn_lifecycle_one_active_per_session; Type: INDEX; Schema: public
--

CREATE UNIQUE INDEX turn_lifecycle_one_active_per_session ON turn_lifecycle USING btree (session_id) WHERE ((state_kind = 'active'::text) AND (NOT delegation_runtime_terminal));


--
-- Name: turn_lifecycle_one_queued_delegation_origin_per_session; Type: INDEX; Schema: public
--

CREATE UNIQUE INDEX turn_lifecycle_one_queued_delegation_origin_per_session ON turn_lifecycle USING btree (session_id) WHERE ((state_kind = 'queued'::text) AND (origin_kind = 'delegation'::text) AND (NOT delegation_runtime_terminal));


--
-- Name: turn_lifecycle_queued_by_session; Type: INDEX; Schema: public
--

CREATE INDEX turn_lifecycle_queued_by_session ON turn_lifecycle USING btree (session_id) WHERE (state_kind = 'queued'::text);


--
-- Name: turn_lifecycle_session_live_reconciliation; Type: INDEX; Schema: public
--

CREATE INDEX turn_lifecycle_session_live_reconciliation ON turn_lifecycle USING btree (session_id, acceptance_position DESC) WHERE ((state_kind = 'terminal'::text) AND (terminal_disposition_kind = 'reconciliation_required'::text) AND (NOT delegation_runtime_terminal));


--
-- Triggers.
--

--
-- Name: accepted_input_content_part accepted_input_content_part_insert_is_creation_local; Type: TRIGGER; Schema: public
--

CREATE TRIGGER accepted_input_content_part_insert_is_creation_local BEFORE INSERT ON accepted_input_content_part FOR EACH ROW EXECUTE FUNCTION reject_content_part_insert_after_parent_transaction();


--
-- Name: accepted_input_content_part accepted_input_content_part_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER accepted_input_content_part_is_append_only BEFORE DELETE OR UPDATE ON accepted_input_content_part FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: accepted_input_content_part accepted_input_content_parts_are_valid; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER accepted_input_content_parts_are_valid AFTER INSERT OR DELETE OR UPDATE ON accepted_input_content_part DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_accepted_input_parts();


--
-- Name: accepted_input accepted_input_content_parts_creation_is_immutable; Type: TRIGGER; Schema: public
--

CREATE TRIGGER accepted_input_content_parts_creation_is_immutable BEFORE UPDATE OF content_parts_creation_xid ON accepted_input FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: accepted_input accepted_input_descendant_scope_is_immutable; Type: TRIGGER; Schema: public
--

CREATE TRIGGER accepted_input_descendant_scope_is_immutable BEFORE UPDATE ON accepted_input FOR EACH ROW EXECUTE FUNCTION reject_accepted_input_descendant_scope_change();


--
-- Name: accepted_input accepted_input_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER accepted_input_is_append_only BEFORE DELETE OR UPDATE ON accepted_input FOR EACH ROW EXECUTE FUNCTION reject_invalid_accepted_input_change();


--
-- Name: accepted_input accepted_input_pending_requires_active_source; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER accepted_input_pending_requires_active_source AFTER INSERT OR UPDATE ON accepted_input DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_pending_steering_active_source();


--
-- Name: accepted_input accepted_input_requires_content_parts; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER accepted_input_requires_content_parts AFTER INSERT ON accepted_input DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_accepted_input_parts();


--
-- Name: accepted_input accepted_input_requires_steering_final_state; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER accepted_input_requires_steering_final_state AFTER INSERT OR DELETE OR UPDATE ON accepted_input DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_accepted_input_steering_final_state();


--
-- Name: accepted_input accepted_input_source_closed; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER accepted_input_source_closed AFTER INSERT OR UPDATE ON accepted_input DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_accepted_input_source();


--
-- Name: accepted_input accepted_input_updates_timeline_fact; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER accepted_input_updates_timeline_fact AFTER INSERT ON accepted_input DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION append_session_timeline_input_bytes();


--
-- Name: input_accepted_outbox_event input_accepted_outbox_event_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER input_accepted_outbox_event_cannot_be_truncated BEFORE TRUNCATE ON input_accepted_outbox_event FOR EACH STATEMENT EXECUTE FUNCTION reject_outbox_table_truncate();


--
-- Name: input_accepted_outbox_event input_accepted_outbox_event_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER input_accepted_outbox_event_is_append_only BEFORE DELETE OR UPDATE ON input_accepted_outbox_event FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: queued_input_origin queued_input_origin_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER queued_input_origin_is_append_only BEFORE DELETE OR UPDATE ON queued_input_origin FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: semantic_transcript_entry semantic_entry_delete_requires_matching_turn_state; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER semantic_entry_delete_requires_matching_turn_state AFTER DELETE ON semantic_transcript_entry DEFERRABLE INITIALLY DEFERRED FOR EACH ROW WHEN ((old.payload_kind <> ALL (ARRAY['delegated_task'::text, 'delegation_message'::text, 'delegation_result'::text]))) EXECUTE FUNCTION require_semantic_entry_turn_state();


--
-- Name: semantic_transcript_entry semantic_entry_requires_matching_turn_state; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER semantic_entry_requires_matching_turn_state AFTER INSERT ON semantic_transcript_entry DEFERRABLE INITIALLY DEFERRED FOR EACH ROW WHEN ((new.payload_kind <> ALL (ARRAY['delegated_task'::text, 'delegation_message'::text, 'delegation_result'::text]))) EXECUTE FUNCTION require_semantic_entry_turn_state();


--
-- Name: semantic_transcript_entry semantic_entry_requires_steering_final_state; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER semantic_entry_requires_steering_final_state AFTER INSERT OR DELETE OR UPDATE ON semantic_transcript_entry DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_semantic_steering_final_state();


--
-- Name: semantic_transcript_entry semantic_entry_update_requires_matching_turn_state; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER semantic_entry_update_requires_matching_turn_state AFTER UPDATE ON semantic_transcript_entry DEFERRABLE INITIALLY DEFERRED FOR EACH ROW WHEN (((old.payload_kind <> ALL (ARRAY['delegated_task'::text, 'delegation_message'::text, 'delegation_result'::text])) OR (new.payload_kind <> ALL (ARRAY['delegated_task'::text, 'delegation_message'::text, 'delegation_result'::text])))) EXECUTE FUNCTION require_semantic_entry_turn_state();


--
-- Name: semantic_transcript_entry semantic_transcript_entry_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER semantic_transcript_entry_is_append_only BEFORE DELETE OR UPDATE ON semantic_transcript_entry FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: semantic_transcript_entry semantic_transcript_entry_updates_timeline_fact; Type: TRIGGER; Schema: public
--

CREATE TRIGGER semantic_transcript_entry_updates_timeline_fact AFTER INSERT ON semantic_transcript_entry FOR EACH ROW EXECUTE FUNCTION append_session_timeline_transcript_bytes();


--
-- Name: submit_input_command_content_part submit_input_command_content_part_insert_is_creation_local; Type: TRIGGER; Schema: public
--

CREATE TRIGGER submit_input_command_content_part_insert_is_creation_local BEFORE INSERT ON submit_input_command_content_part FOR EACH ROW EXECUTE FUNCTION reject_content_part_insert_after_parent_transaction();


--
-- Name: submit_input_command_content_part submit_input_command_content_part_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER submit_input_command_content_part_is_append_only BEFORE DELETE OR UPDATE ON submit_input_command_content_part FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: submit_input_command_content_part submit_input_command_content_parts_are_valid; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER submit_input_command_content_parts_are_valid AFTER INSERT OR DELETE OR UPDATE ON submit_input_command_content_part DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_submit_input_command_parts();


--
-- Name: submit_input_command submit_input_command_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER submit_input_command_is_append_only BEFORE DELETE OR UPDATE ON submit_input_command FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: submit_input_command submit_input_command_requires_content_parts; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER submit_input_command_requires_content_parts AFTER INSERT ON submit_input_command DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_submit_input_command_parts();


--
-- Name: submit_input_command submit_input_command_requires_correlated_effect; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER submit_input_command_requires_correlated_effect AFTER INSERT ON submit_input_command DEFERRABLE INITIALLY DEFERRED FOR EACH ROW WHEN ((NOT (((new.result_kind = 'applied'::text) AND (new.delivery_kind = 'interrupt'::text)) OR COALESCE((new.rejection_kind = ANY (ARRAY['safe_point_unavailable_while_stopping'::text, 'interrupt_already_applied'::text, 'interrupt_unavailable_while_awaiting_approval'::text])), false)))) EXECUTE FUNCTION require_submit_input_legacy_effect_correlation();


--
-- Name: submit_input_command submit_input_command_requires_interrupt_effect; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER submit_input_command_requires_interrupt_effect AFTER INSERT ON submit_input_command DEFERRABLE INITIALLY DEFERRED FOR EACH ROW WHEN ((((new.result_kind = 'applied'::text) AND (new.delivery_kind = 'interrupt'::text)) OR COALESCE((new.rejection_kind = ANY (ARRAY['safe_point_unavailable_while_stopping'::text, 'interrupt_already_applied'::text, 'interrupt_unavailable_while_awaiting_approval'::text])), false))) EXECUTE FUNCTION require_interrupt_submit_input_effect_correlation();


--
-- Name: turn_activated_outbox_event turn_activated_outbox_event_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER turn_activated_outbox_event_cannot_be_truncated BEFORE TRUNCATE ON turn_activated_outbox_event FOR EACH STATEMENT EXECUTE FUNCTION reject_outbox_table_truncate();


--
-- Name: turn_activated_outbox_event turn_activated_outbox_event_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER turn_activated_outbox_event_is_append_only BEFORE DELETE OR UPDATE ON turn_activated_outbox_event FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: turn_attempt turn_attempt_changes_are_guarded; Type: TRIGGER; Schema: public
--

CREATE TRIGGER turn_attempt_changes_are_guarded BEFORE INSERT OR DELETE OR UPDATE ON turn_attempt FOR EACH ROW EXECUTE FUNCTION reject_turn_attempt_invalid_change();


--
-- Name: turn_attempt turn_attempt_rechecks_turn_runner_recovery; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER turn_attempt_rechecks_turn_runner_recovery AFTER INSERT OR DELETE OR UPDATE ON turn_attempt DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_turn_runner_recovery_complete();


--
-- Name: turn_attempt turn_attempt_requires_complete_final_state; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER turn_attempt_requires_complete_final_state AFTER INSERT OR DELETE OR UPDATE ON turn_attempt DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_turn_attempt_final_state();


--
-- Name: turn_attempt turn_attempt_requires_failed_terminal_execution; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER turn_attempt_requires_failed_terminal_execution AFTER INSERT OR DELETE OR UPDATE ON turn_attempt DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_failed_terminal_execution_final_state();


--
-- Name: turn_attempt turn_attempt_requires_interrupt_proof; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER turn_attempt_requires_interrupt_proof AFTER INSERT OR DELETE OR UPDATE ON turn_attempt DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_interrupt_attempt_proof();


--
-- Name: turn_cancelled_outbox_event turn_cancelled_outbox_event_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER turn_cancelled_outbox_event_cannot_be_truncated BEFORE TRUNCATE ON turn_cancelled_outbox_event FOR EACH STATEMENT EXECUTE FUNCTION reject_outbox_table_truncate();


--
-- Name: turn_cancelled_outbox_event turn_cancelled_outbox_event_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER turn_cancelled_outbox_event_is_append_only BEFORE DELETE OR UPDATE ON turn_cancelled_outbox_event FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: turn_completed_outbox_event turn_completed_outbox_event_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER turn_completed_outbox_event_cannot_be_truncated BEFORE TRUNCATE ON turn_completed_outbox_event FOR EACH STATEMENT EXECUTE FUNCTION reject_outbox_table_truncate();


--
-- Name: turn_completed_outbox_event turn_completed_outbox_event_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER turn_completed_outbox_event_is_append_only BEFORE DELETE OR UPDATE ON turn_completed_outbox_event FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: turn_failed_outbox_event turn_failed_outbox_event_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER turn_failed_outbox_event_cannot_be_truncated BEFORE TRUNCATE ON turn_failed_outbox_event FOR EACH STATEMENT EXECUTE FUNCTION reject_outbox_table_truncate();


--
-- Name: turn_failed_outbox_event turn_failed_outbox_event_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER turn_failed_outbox_event_is_append_only BEFORE DELETE OR UPDATE ON turn_failed_outbox_event FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: turn_lifecycle turn_lifecycle_changes_are_guarded; Type: TRIGGER; Schema: public
--

CREATE TRIGGER turn_lifecycle_changes_are_guarded BEFORE INSERT OR DELETE OR UPDATE ON turn_lifecycle FOR EACH ROW EXECUTE FUNCTION reject_turn_lifecycle_invalid_change();


--
-- Name: turn_lifecycle turn_lifecycle_delegation_runtime_terminal_is_monotonic; Type: TRIGGER; Schema: public
--

CREATE TRIGGER turn_lifecycle_delegation_runtime_terminal_is_monotonic BEFORE UPDATE OF delegation_runtime_terminal ON turn_lifecycle FOR EACH ROW EXECUTE FUNCTION reject_turn_delegation_runtime_terminal_reversal();


--
-- Name: turn_lifecycle turn_lifecycle_delegation_runtime_terminal_requires_proof; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER turn_lifecycle_delegation_runtime_terminal_requires_proof AFTER INSERT OR UPDATE OF delegation_runtime_terminal ON turn_lifecycle DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_turn_delegation_runtime_terminal_proof();


--
-- Name: turn_lifecycle turn_lifecycle_origin_kind_is_immutable; Type: TRIGGER; Schema: public
--

CREATE TRIGGER turn_lifecycle_origin_kind_is_immutable BEFORE UPDATE ON turn_lifecycle FOR EACH ROW EXECUTE FUNCTION reject_turn_lifecycle_origin_kind_change();


--
-- Name: turn_lifecycle turn_lifecycle_requires_child_wait_mode; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER turn_lifecycle_requires_child_wait_mode AFTER INSERT OR UPDATE ON turn_lifecycle DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_turn_child_wait_mode();


--
-- Name: turn_lifecycle turn_lifecycle_requires_complete_final_state; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER turn_lifecycle_requires_complete_final_state AFTER INSERT OR DELETE OR UPDATE ON turn_lifecycle DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_turn_lifecycle_final_state();


--
-- Name: turn_lifecycle turn_lifecycle_requires_failed_terminal_execution; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER turn_lifecycle_requires_failed_terminal_execution AFTER INSERT OR DELETE OR UPDATE ON turn_lifecycle DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_failed_terminal_execution_final_state();


--
-- Name: turn_lifecycle turn_lifecycle_runner_recovery_is_complete; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER turn_lifecycle_runner_recovery_is_complete AFTER INSERT OR DELETE OR UPDATE ON turn_lifecycle DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_turn_runner_recovery_complete();


--
-- Name: turn_lifecycle turn_lifecycle_updates_timeline_fact; Type: TRIGGER; Schema: public
--

CREATE TRIGGER turn_lifecycle_updates_timeline_fact AFTER INSERT OR UPDATE OF state_kind, delegation_runtime_terminal ON turn_lifecycle FOR EACH ROW EXECUTE FUNCTION update_session_timeline_work_fact();


--
-- Name: turn_model_settings_resolved turn_model_settings_resolved_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER turn_model_settings_resolved_is_append_only BEFORE DELETE OR UPDATE ON turn_model_settings_resolved FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: turn_model_settings_resolved_outbox_event turn_model_settings_resolved_outbox_event_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER turn_model_settings_resolved_outbox_event_cannot_be_truncated BEFORE TRUNCATE ON turn_model_settings_resolved_outbox_event FOR EACH STATEMENT EXECUTE FUNCTION reject_outbox_table_truncate();


--
-- Name: turn_model_settings_resolved_outbox_event turn_model_settings_resolved_outbox_event_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER turn_model_settings_resolved_outbox_event_is_append_only BEFORE DELETE OR UPDATE ON turn_model_settings_resolved_outbox_event FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: turn_reconciliation_required_outbox_event turn_reconciliation_required_outbox_event_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER turn_reconciliation_required_outbox_event_cannot_be_truncated BEFORE TRUNCATE ON turn_reconciliation_required_outbox_event FOR EACH STATEMENT EXECUTE FUNCTION reject_outbox_table_truncate();


--
-- Name: turn_reconciliation_required_outbox_event turn_reconciliation_required_outbox_event_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER turn_reconciliation_required_outbox_event_is_append_only BEFORE DELETE OR UPDATE ON turn_reconciliation_required_outbox_event FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: turn_refused_outbox_event turn_refused_outbox_event_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER turn_refused_outbox_event_cannot_be_truncated BEFORE TRUNCATE ON turn_refused_outbox_event FOR EACH STATEMENT EXECUTE FUNCTION reject_outbox_table_truncate();


--
-- Name: turn_refused_outbox_event turn_refused_outbox_event_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER turn_refused_outbox_event_is_append_only BEFORE DELETE OR UPDATE ON turn_refused_outbox_event FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: turn_restart_recovery_origin turn_restart_recovery_origin_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER turn_restart_recovery_origin_is_append_only BEFORE DELETE OR UPDATE ON turn_restart_recovery_origin FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: turn_restart_recovery_origin turn_restart_recovery_origin_reject_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER turn_restart_recovery_origin_reject_truncate BEFORE TRUNCATE ON turn_restart_recovery_origin FOR EACH STATEMENT EXECUTE FUNCTION reject_turn_restart_recovery_origin_truncate();


--
-- Name: turn_runner_recovery_interrupt_effect turn_runner_recovery_interrupt_effect_is_authorized; Type: TRIGGER; Schema: public
--

CREATE TRIGGER turn_runner_recovery_interrupt_effect_is_authorized BEFORE INSERT ON turn_runner_recovery_interrupt_effect FOR EACH ROW EXECUTE FUNCTION guard_turn_runner_recovery_interrupt_effect();


--
-- Name: turn_runner_recovery_interrupt_effect turn_runner_recovery_interrupt_effect_is_immutable; Type: TRIGGER; Schema: public
--

CREATE TRIGGER turn_runner_recovery_interrupt_effect_is_immutable AFTER DELETE OR UPDATE ON turn_runner_recovery_interrupt_effect FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: turn_runner_recovery_interrupt_effect turn_runner_recovery_interrupt_effect_rejects_truncate; Type: TRIGGER; Schema: public
--

CREATE TRIGGER turn_runner_recovery_interrupt_effect_rejects_truncate BEFORE TRUNCATE ON turn_runner_recovery_interrupt_effect FOR EACH STATEMENT EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: turn_lifecycle turn_terminal_requires_closed_pending_steering; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER turn_terminal_requires_closed_pending_steering AFTER INSERT OR UPDATE ON turn_lifecycle DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_pending_steering_active_source();


--
-- Foreign keys.
--

--
-- Name: accepted_input accepted_input_command_scope_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY accepted_input
    ADD CONSTRAINT accepted_input_command_scope_fk FOREIGN KEY (accepting_command_id, descendant_scope) REFERENCES submit_input_command(command_id, descendant_scope) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: accepted_input accepted_input_command_settings_result_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY accepted_input
    ADD CONSTRAINT accepted_input_command_settings_result_fk FOREIGN KEY (accepting_command_id, accepted_input_id, session_id, model_settings_override) REFERENCES submit_input_command(command_id, result_accepted_input_id, result_session_id, model_settings_override) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: accepted_input_content_part accepted_input_content_part_input_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY accepted_input_content_part
    ADD CONSTRAINT accepted_input_content_part_input_fk FOREIGN KEY (accepted_input_id) REFERENCES accepted_input(accepted_input_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: accepted_input accepted_input_expected_active_turn_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY accepted_input
    ADD CONSTRAINT accepted_input_expected_active_turn_fk FOREIGN KEY (expected_active_turn_id, session_id) REFERENCES turn_lifecycle(turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: accepted_input accepted_input_general_command_result_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY accepted_input
    ADD CONSTRAINT accepted_input_general_command_result_fk FOREIGN KEY (accepting_command_id, accepted_input_id, session_id) REFERENCES submit_input_command(command_id, result_accepted_input_id, result_session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: accepted_input accepted_input_queued_origin_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY accepted_input
    ADD CONSTRAINT accepted_input_queued_origin_fk FOREIGN KEY (accepted_input_id, session_id, acceptance_position, origin_turn_id) REFERENCES queued_input_origin(accepted_input_id, session_id, acceptance_position, turn_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: accepted_input accepted_input_session_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY accepted_input
    ADD CONSTRAINT accepted_input_session_fk FOREIGN KEY (session_id) REFERENCES session(session_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: automatic_reconciliation_attempt automatic_model_call_reconciliation_attempt_turn_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY automatic_reconciliation_attempt
    ADD CONSTRAINT automatic_model_call_reconciliation_attempt_turn_id_fkey FOREIGN KEY (turn_id) REFERENCES automatic_reconciliation(turn_id);


--
-- Name: automatic_reconciliation automatic_model_call_reconciliation_turn_id_session_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY automatic_reconciliation
    ADD CONSTRAINT automatic_model_call_reconciliation_turn_id_session_id_fkey FOREIGN KEY (turn_id, session_id) REFERENCES turn_lifecycle(turn_id, session_id);


--
-- Name: input_accepted_outbox_event input_accepted_outbox_header_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY input_accepted_outbox_event
    ADD CONSTRAINT input_accepted_outbox_header_fk FOREIGN KEY (event_sequence, event_kind, storage_version, session_id) REFERENCES outbox_event(event_sequence, event_kind, storage_version, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: input_accepted_outbox_event input_accepted_outbox_origin_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY input_accepted_outbox_event
    ADD CONSTRAINT input_accepted_outbox_origin_fk FOREIGN KEY (accepted_input_id, session_id, acceptance_position, turn_id) REFERENCES accepted_input(accepted_input_id, session_id, acceptance_position, origin_turn_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: queued_input_origin queued_input_origin_accepted_input_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY queued_input_origin
    ADD CONSTRAINT queued_input_origin_accepted_input_fk FOREIGN KEY (accepted_input_id, session_id, acceptance_position, turn_id) REFERENCES accepted_input(accepted_input_id, session_id, acceptance_position, origin_turn_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: queued_input_origin queued_input_origin_configuration_source_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY queued_input_origin
    ADD CONSTRAINT queued_input_origin_configuration_source_fk FOREIGN KEY (source_configuration_turn_id, session_id) REFERENCES queued_input_origin(turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: queued_input_origin queued_input_origin_defaults_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY queued_input_origin
    ADD CONSTRAINT queued_input_origin_defaults_fk FOREIGN KEY (session_id, defaults_version) REFERENCES session_defaults_version(session_id, version) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: queued_input_origin queued_input_origin_interrupt_predecessor_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY queued_input_origin
    ADD CONSTRAINT queued_input_origin_interrupt_predecessor_fk FOREIGN KEY (interrupt_predecessor_turn_id, session_id) REFERENCES turn_lifecycle(turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: queued_input_origin queued_input_origin_turn_lifecycle_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY queued_input_origin
    ADD CONSTRAINT queued_input_origin_turn_lifecycle_fk FOREIGN KEY (accepted_input_id, session_id, acceptance_position, turn_id) REFERENCES turn_lifecycle(origin_accepted_input_id, session_id, acceptance_position, turn_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: semantic_transcript_entry semantic_transcript_entry_cancelled_turn_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY semantic_transcript_entry
    ADD CONSTRAINT semantic_transcript_entry_cancelled_turn_fk FOREIGN KEY (cancelled_turn_id, source_session_id) REFERENCES turn_lifecycle(turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: semantic_transcript_entry semantic_transcript_entry_completed_turn_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY semantic_transcript_entry
    ADD CONSTRAINT semantic_transcript_entry_completed_turn_fk FOREIGN KEY (completed_turn_id, source_session_id) REFERENCES turn_lifecycle(turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: semantic_transcript_entry semantic_transcript_entry_context_summary_first_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY semantic_transcript_entry
    ADD CONSTRAINT semantic_transcript_entry_context_summary_first_fk FOREIGN KEY (context_summary_first_source_session_id, context_summary_first_entry_id) REFERENCES semantic_transcript_entry(source_session_id, semantic_entry_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: semantic_transcript_entry semantic_transcript_entry_context_summary_through_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY semantic_transcript_entry
    ADD CONSTRAINT semantic_transcript_entry_context_summary_through_fk FOREIGN KEY (context_summary_through_source_session_id, context_summary_through_entry_id) REFERENCES semantic_transcript_entry(source_session_id, semantic_entry_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: semantic_transcript_entry semantic_transcript_entry_failed_turn_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY semantic_transcript_entry
    ADD CONSTRAINT semantic_transcript_entry_failed_turn_fk FOREIGN KEY (failed_turn_id, source_session_id) REFERENCES turn_lifecycle(turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: semantic_transcript_entry semantic_transcript_entry_model_identity_turn_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY semantic_transcript_entry
    ADD CONSTRAINT semantic_transcript_entry_model_identity_turn_fk FOREIGN KEY (model_identity_turn_id, source_session_id) REFERENCES turn_lifecycle(turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: semantic_transcript_entry semantic_transcript_entry_origin_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY semantic_transcript_entry
    ADD CONSTRAINT semantic_transcript_entry_origin_fk FOREIGN KEY (origin_accepted_input_id, source_session_id) REFERENCES accepted_input(accepted_input_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: semantic_transcript_entry semantic_transcript_entry_source_session_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY semantic_transcript_entry
    ADD CONSTRAINT semantic_transcript_entry_source_session_fk FOREIGN KEY (source_session_id) REFERENCES session(session_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: semantic_transcript_entry semantic_transcript_entry_steering_source_turn_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY semantic_transcript_entry
    ADD CONSTRAINT semantic_transcript_entry_steering_source_turn_fk FOREIGN KEY (steering_source_turn_id, source_session_id) REFERENCES turn_lifecycle(turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: submit_input_command submit_input_command_actual_active_turn_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY submit_input_command
    ADD CONSTRAINT submit_input_command_actual_active_turn_fk FOREIGN KEY (result_actual_active_turn_id, result_session_id) REFERENCES turn_lifecycle(turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: submit_input_command submit_input_command_applied_effect_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY submit_input_command
    ADD CONSTRAINT submit_input_command_applied_effect_fk FOREIGN KEY (result_accepted_input_id, result_session_id, result_turn_id) REFERENCES accepted_input(accepted_input_id, session_id, origin_turn_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: submit_input_command_content_part submit_input_command_content_part_command_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY submit_input_command_content_part
    ADD CONSTRAINT submit_input_command_content_part_command_fk FOREIGN KEY (command_id) REFERENCES submit_input_command(command_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: submit_input_command submit_input_command_current_defaults_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY submit_input_command
    ADD CONSTRAINT submit_input_command_current_defaults_fk FOREIGN KEY (result_session_id, result_current_defaults_version) REFERENCES session_defaults_version(session_id, version) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: submit_input_command submit_input_command_existing_interrupt_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY submit_input_command
    ADD CONSTRAINT submit_input_command_existing_interrupt_fk FOREIGN KEY (result_existing_interrupt_command_id) REFERENCES submit_input_command(command_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: submit_input_command submit_input_command_general_applied_effect_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY submit_input_command
    ADD CONSTRAINT submit_input_command_general_applied_effect_fk FOREIGN KEY (command_id, result_accepted_input_id, result_session_id) REFERENCES accepted_input(accepting_command_id, accepted_input_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: submit_input_command submit_input_command_last_position_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY submit_input_command
    ADD CONSTRAINT submit_input_command_last_position_fk FOREIGN KEY (result_session_id, result_last_position) REFERENCES accepted_input(session_id, acceptance_position) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: submit_input_command submit_input_command_pending_effect_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY submit_input_command
    ADD CONSTRAINT submit_input_command_pending_effect_fk FOREIGN KEY (result_accepted_input_id, result_session_id, result_actual_active_turn_id) REFERENCES accepted_input(accepted_input_id, session_id, expected_active_turn_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: submit_input_command submit_input_command_registry_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY submit_input_command
    ADD CONSTRAINT submit_input_command_registry_fk FOREIGN KEY (command_id, command_kind, storage_version) REFERENCES durable_command(command_id, command_kind, storage_version) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: submit_input_command submit_input_command_selected_defaults_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY submit_input_command
    ADD CONSTRAINT submit_input_command_selected_defaults_fk FOREIGN KEY (result_session_id, result_selected_defaults_version) REFERENCES session_defaults_version(session_id, version) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: turn_activated_outbox_event turn_activated_outbox_attempt_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_activated_outbox_event
    ADD CONSTRAINT turn_activated_outbox_attempt_fk FOREIGN KEY (current_attempt_id, turn_id, session_id) REFERENCES turn_attempt(turn_attempt_id, turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: turn_activated_outbox_event turn_activated_outbox_header_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_activated_outbox_event
    ADD CONSTRAINT turn_activated_outbox_header_fk FOREIGN KEY (event_sequence, event_kind, storage_version, session_id) REFERENCES outbox_event(event_sequence, event_kind, storage_version, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: turn_attempt turn_attempt_continued_from_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_attempt
    ADD CONSTRAINT turn_attempt_continued_from_fk FOREIGN KEY (continued_from_attempt_id, turn_id, session_id) REFERENCES turn_attempt(turn_attempt_id, turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: turn_attempt turn_attempt_interrupt_command_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_attempt
    ADD CONSTRAINT turn_attempt_interrupt_command_fk FOREIGN KEY (interrupt_command_id) REFERENCES submit_input_command(command_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: turn_attempt turn_attempt_interrupt_predecessor_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_attempt
    ADD CONSTRAINT turn_attempt_interrupt_predecessor_fk FOREIGN KEY (interrupt_predecessor_turn_id, session_id) REFERENCES turn_lifecycle(turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: turn_attempt turn_attempt_turn_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_attempt
    ADD CONSTRAINT turn_attempt_turn_fk FOREIGN KEY (turn_id, session_id) REFERENCES turn_lifecycle(turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: turn_cancelled_outbox_event turn_cancelled_outbox_entry_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_cancelled_outbox_event
    ADD CONSTRAINT turn_cancelled_outbox_entry_fk FOREIGN KEY (session_id, cancellation_entry_id) REFERENCES semantic_transcript_entry(source_session_id, semantic_entry_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: turn_cancelled_outbox_event turn_cancelled_outbox_header_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_cancelled_outbox_event
    ADD CONSTRAINT turn_cancelled_outbox_header_fk FOREIGN KEY (event_sequence, event_kind, storage_version, session_id) REFERENCES outbox_event(event_sequence, event_kind, storage_version, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: turn_cancelled_outbox_event turn_cancelled_outbox_turn_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_cancelled_outbox_event
    ADD CONSTRAINT turn_cancelled_outbox_turn_fk FOREIGN KEY (turn_id, session_id) REFERENCES turn_lifecycle(turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: turn_completed_outbox_event turn_completed_outbox_entry_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_completed_outbox_event
    ADD CONSTRAINT turn_completed_outbox_entry_fk FOREIGN KEY (session_id, completion_entry_id) REFERENCES semantic_transcript_entry(source_session_id, semantic_entry_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: turn_completed_outbox_event turn_completed_outbox_header_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_completed_outbox_event
    ADD CONSTRAINT turn_completed_outbox_header_fk FOREIGN KEY (event_sequence, event_kind, storage_version, session_id) REFERENCES outbox_event(event_sequence, event_kind, storage_version, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: turn_completed_outbox_event turn_completed_outbox_turn_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_completed_outbox_event
    ADD CONSTRAINT turn_completed_outbox_turn_fk FOREIGN KEY (turn_id, session_id) REFERENCES turn_lifecycle(turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: turn_failed_outbox_event turn_failed_outbox_event_failure_entry_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_failed_outbox_event
    ADD CONSTRAINT turn_failed_outbox_event_failure_entry_fk FOREIGN KEY (session_id, failure_entry_id) REFERENCES semantic_transcript_entry(source_session_id, semantic_entry_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: turn_failed_outbox_event turn_failed_outbox_event_header_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_failed_outbox_event
    ADD CONSTRAINT turn_failed_outbox_event_header_fk FOREIGN KEY (event_sequence, event_kind, storage_version, session_id) REFERENCES outbox_event(event_sequence, event_kind, storage_version, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: turn_failed_outbox_event turn_failed_outbox_event_turn_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_failed_outbox_event
    ADD CONSTRAINT turn_failed_outbox_event_turn_fk FOREIGN KEY (turn_id, session_id) REFERENCES turn_lifecycle(turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: turn_lifecycle turn_lifecycle_current_attempt_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_lifecycle
    ADD CONSTRAINT turn_lifecycle_current_attempt_fk FOREIGN KEY (current_attempt_id, turn_id, session_id) REFERENCES turn_attempt(turn_attempt_id, turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: turn_lifecycle turn_lifecycle_predecessor_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_lifecycle
    ADD CONSTRAINT turn_lifecycle_predecessor_fk FOREIGN KEY (immediate_predecessor_turn_id, session_id) REFERENCES turn_lifecycle(turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: turn_lifecycle turn_lifecycle_queued_origin_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_lifecycle
    ADD CONSTRAINT turn_lifecycle_queued_origin_fk FOREIGN KEY (origin_accepted_input_id, session_id, acceptance_position, turn_id) REFERENCES queued_input_origin(accepted_input_id, session_id, acceptance_position, turn_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: turn_lifecycle turn_lifecycle_session_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_lifecycle
    ADD CONSTRAINT turn_lifecycle_session_fk FOREIGN KEY (session_id) REFERENCES session(session_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: turn_lifecycle turn_lifecycle_terminal_attempt_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_lifecycle
    ADD CONSTRAINT turn_lifecycle_terminal_attempt_fk FOREIGN KEY (terminal_attempt_id, turn_id, session_id) REFERENCES turn_attempt(turn_attempt_id, turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: turn_model_settings_resolved turn_model_settings_resolved_defaults_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_model_settings_resolved
    ADD CONSTRAINT turn_model_settings_resolved_defaults_fk FOREIGN KEY (session_id, defaults_version) REFERENCES session_defaults_version(session_id, version) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: turn_model_settings_resolved turn_model_settings_resolved_input_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_model_settings_resolved
    ADD CONSTRAINT turn_model_settings_resolved_input_fk FOREIGN KEY (accepted_input_id, session_id, turn_id) REFERENCES accepted_input(accepted_input_id, session_id, origin_turn_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: turn_model_settings_resolved_outbox_event turn_model_settings_resolved_outbox_event_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_model_settings_resolved_outbox_event
    ADD CONSTRAINT turn_model_settings_resolved_outbox_event_fk FOREIGN KEY (accepted_input_id, session_id) REFERENCES turn_model_settings_resolved(accepted_input_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: turn_model_settings_resolved_outbox_event turn_model_settings_resolved_outbox_header_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_model_settings_resolved_outbox_event
    ADD CONSTRAINT turn_model_settings_resolved_outbox_header_fk FOREIGN KEY (event_sequence, event_kind, storage_version, session_id) REFERENCES outbox_event(event_sequence, event_kind, storage_version, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: turn_model_settings_resolved turn_model_settings_resolved_turn_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_model_settings_resolved
    ADD CONSTRAINT turn_model_settings_resolved_turn_fk FOREIGN KEY (turn_id, session_id) REFERENCES turn_lifecycle(turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: turn_reconciliation_required_outbox_event turn_reconciliation_required_outbox_header_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_reconciliation_required_outbox_event
    ADD CONSTRAINT turn_reconciliation_required_outbox_header_fk FOREIGN KEY (event_sequence, event_kind, storage_version, session_id) REFERENCES outbox_event(event_sequence, event_kind, storage_version, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: turn_reconciliation_required_outbox_event turn_reconciliation_required_outbox_turn_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_reconciliation_required_outbox_event
    ADD CONSTRAINT turn_reconciliation_required_outbox_turn_fk FOREIGN KEY (turn_id, session_id) REFERENCES turn_lifecycle(turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: turn_refused_outbox_event turn_refused_outbox_header_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_refused_outbox_event
    ADD CONSTRAINT turn_refused_outbox_header_fk FOREIGN KEY (event_sequence, event_kind, storage_version, session_id) REFERENCES outbox_event(event_sequence, event_kind, storage_version, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: turn_refused_outbox_event turn_refused_outbox_turn_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_refused_outbox_event
    ADD CONSTRAINT turn_refused_outbox_turn_fk FOREIGN KEY (turn_id, session_id) REFERENCES turn_lifecycle(turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: turn_restart_recovery_origin turn_restart_recovery_origin_turn_id_session_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_restart_recovery_origin
    ADD CONSTRAINT turn_restart_recovery_origin_turn_id_session_id_fkey FOREIGN KEY (turn_id, session_id) REFERENCES turn_lifecycle(turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: turn_runner_recovery_interrupt_effect turn_runner_recovery_interrup_yielded_turn_attempt_id_turn_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_runner_recovery_interrupt_effect
    ADD CONSTRAINT turn_runner_recovery_interrup_yielded_turn_attempt_id_turn_fkey FOREIGN KEY (yielded_turn_attempt_id, turn_id, session_id) REFERENCES turn_attempt(turn_attempt_id, turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: turn_runner_recovery_interrupt_effect turn_runner_recovery_interrupt_effect_command_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_runner_recovery_interrupt_effect
    ADD CONSTRAINT turn_runner_recovery_interrupt_effect_command_id_fkey FOREIGN KEY (command_id) REFERENCES submit_input_command(command_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: turn_runner_recovery_interrupt_effect turn_runner_recovery_interrupt_effect_turn_id_session_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY turn_runner_recovery_interrupt_effect
    ADD CONSTRAINT turn_runner_recovery_interrupt_effect_turn_id_session_id_fkey FOREIGN KEY (turn_id, session_id) REFERENCES turn_lifecycle(turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


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
    -- the server default captured at creation time by SET search_path FROM CURRENT
    FOREACH signature IN ARRAY ARRAY[
        'accepted_input_projected_text(uuid)'
    ] LOOP
        EXECUTE format('ALTER FUNCTION %s SET search_path TO "$user", %I',
                   signature, current_schema);
    END LOOP;
    -- the canonical restore-safe pin: the migration-selected schema, then pg_catalog, then pg_temp
    FOREACH signature IN ARRAY ARRAY[
        'assert_reconciliation_required_turn_final_state(uuid)'
    ] LOOP
        EXECUTE format('ALTER FUNCTION %s SET search_path TO %I, pg_catalog, pg_temp',
                   signature, current_schema);
    END LOOP;
END
$search_path_pins$;


--
-- Singleton bootstrap rows: the two automatic-reconciliation cursors, read by
-- discovery before anything can write them (the other seeded singletons are
-- in 202609010000_core.sql).
--

INSERT INTO automatic_reconciliation_discovery_state (singleton, after_turn_id, high_turn_id) VALUES (true, NULL, NULL);
INSERT INTO automatic_reconciliation_supersession_state (singleton, after_turn_id, high_turn_id) VALUES (true, NULL, NULL);


--
-- Restore body validation for anything applied after this file.
--

RESET check_function_bodies;
