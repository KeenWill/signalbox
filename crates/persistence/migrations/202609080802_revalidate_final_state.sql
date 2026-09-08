CREATE OR REPLACE FUNCTION assert_turn_lifecycle_final_state(checked_turn_id uuid) RETURNS void
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF EXISTS (
        SELECT 1
          FROM turn_lifecycle
         WHERE turn_id = checked_turn_id
           AND state_kind = 'terminal'
           AND terminal_disposition_kind = 'retired'
    ) THEN
        IF (
            SELECT count(*)
              FROM turn_terminal_outbox_event
             WHERE turn_id = checked_turn_id
               AND disposition_kind = 'retired'
        ) <> 1 THEN
            RAISE EXCEPTION 'retired turn % lacks its terminal event', checked_turn_id
                USING ERRCODE = '23514';
        END IF;
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

CREATE OR REPLACE FUNCTION assert_model_call_final_state(checked_model_call_id uuid) RETURNS void
    LANGUAGE plpgsql
    AS $$
DECLARE
    successor_attempt_id uuid;
    predecessor_turn_id uuid;
    predecessor_session_id uuid;
    predecessor_attempt_id uuid;
    predecessor_state text;
    predecessor_disposition text;
    predecessor_attempt_state text;
    predecessor_attempt_disposition text;
    successor_state text;
    successor_continuation uuid;
    lifecycle_state text;
    lifecycle_phase text;
    lifecycle_attempt uuid;
BEGIN
    SELECT
        successor.successor_turn_attempt_id,
        predecessor.turn_id,
        predecessor.session_id,
        predecessor.turn_attempt_id,
        predecessor.state_kind,
        predecessor.terminal_disposition_kind,
        predecessor_attempt.state_kind,
        predecessor_attempt.end_disposition,
        successor_attempt.state_kind,
        successor_attempt.continued_from_attempt_id,
        lifecycle.state_kind,
        lifecycle.active_phase_kind,
        lifecycle.current_attempt_id
      INTO
        successor_attempt_id,
        predecessor_turn_id,
        predecessor_session_id,
        predecessor_attempt_id,
        predecessor_state,
        predecessor_disposition,
        predecessor_attempt_state,
        predecessor_attempt_disposition,
        successor_state,
        successor_continuation,
        lifecycle_state,
        lifecycle_phase,
        lifecycle_attempt
      FROM credential_pool_availability_successor AS successor
      JOIN model_call AS predecessor
        ON predecessor.model_call_id = successor.predecessor_model_call_id
      JOIN turn_attempt AS predecessor_attempt
        ON predecessor_attempt.turn_attempt_id = predecessor.turn_attempt_id
       AND predecessor_attempt.turn_id = predecessor.turn_id
       AND predecessor_attempt.session_id = predecessor.session_id
      JOIN turn_attempt AS successor_attempt
        ON successor_attempt.turn_attempt_id = successor.successor_turn_attempt_id
       AND successor_attempt.turn_id = predecessor.turn_id
       AND successor_attempt.session_id = predecessor.session_id
      JOIN turn_lifecycle AS lifecycle
        ON lifecycle.turn_id = predecessor.turn_id
       AND lifecycle.session_id = predecessor.session_id
     WHERE successor.predecessor_model_call_id = checked_model_call_id;

    IF FOUND THEN
        IF predecessor_state IS DISTINCT FROM 'terminal'
           OR predecessor_disposition IS DISTINCT FROM 'known_failed'
           OR predecessor_attempt_state IS DISTINCT FROM 'ended'
           OR predecessor_attempt_disposition IS DISTINCT FROM 'known_failure'
           OR successor_state IS DISTINCT FROM 'prepared'
           OR successor_continuation IS DISTINCT FROM predecessor_attempt_id
           OR lifecycle_state IS DISTINCT FROM 'active'
           OR lifecycle_phase IS DISTINCT FROM 'running'
           OR lifecycle_attempt IS DISTINCT FROM successor_attempt_id
        THEN
            RAISE EXCEPTION 'availability predecessor lacks its exact successor state'
                USING ERRCODE = '23514';
        END IF;
        RETURN;
    END IF;

    IF EXISTS (
        SELECT 1
          FROM model_call AS call
          JOIN credential_pool_availability_successor AS successor
            ON successor.successor_turn_attempt_id = call.turn_attempt_id
         WHERE call.model_call_id = checked_model_call_id
    ) THEN
        -- A terminal availability successor with no later availability
        -- successor may have yielded into the ordinary tool-round lifecycle,
        -- or ended ambiguously and parked its still-active turn for model-call
        -- recovery. Preserve the availability lineage checks here, then
        -- delegate the terminal lifecycle shape to the validator that owns
        -- both of those active shapes.
        IF EXISTS (
            SELECT 1
              FROM model_call AS call
              JOIN credential_pool_availability_successor AS successor
                ON successor.successor_turn_attempt_id = call.turn_attempt_id
              JOIN model_call AS predecessor
                ON predecessor.model_call_id = successor.predecessor_model_call_id
             WHERE call.model_call_id = checked_model_call_id
               AND call.turn_id = predecessor.turn_id
               AND call.session_id = predecessor.session_id
               AND call.resolved_provider_model_identity_id =
                   predecessor.resolved_provider_model_identity_id
               AND ROW(
                    call.selection_kind,
                    call.direct_model_selection_id,
                    call.frozen_model_alias_id,
                    call.frozen_alias_selected_direct_id
               ) IS NOT DISTINCT FROM ROW(
                    predecessor.selection_kind,
                    predecessor.direct_model_selection_id,
                    predecessor.frozen_model_alias_id,
                    predecessor.frozen_alias_selected_direct_id
               )
               AND call.state_kind = 'terminal'
               AND (
                    EXISTS (
                        SELECT 1
                          FROM tool_round AS round
                         WHERE round.producing_model_call_id = call.model_call_id
                    )
                    OR EXISTS (
                        SELECT 1
                          FROM turn_lifecycle AS waiting
                         WHERE waiting.turn_id = call.turn_id
                           AND waiting.session_id = call.session_id
                           AND waiting.state_kind = 'active'
                           AND waiting.active_phase_kind =
                               'awaiting_model_call_recovery'
                           AND waiting.recovery_model_call_id = call.model_call_id
                    )
               )
               AND NOT EXISTS (
                    SELECT 1
                      FROM credential_pool_availability_successor AS later
                     WHERE later.predecessor_model_call_id = call.model_call_id
               )
        ) THEN
            PERFORM assert_model_call_final_state_before_credential_pools(
                checked_model_call_id
            );
            RETURN;
        END IF;

        IF NOT EXISTS (
            SELECT 1
              FROM model_call AS call
              JOIN credential_pool_availability_successor AS successor
                ON successor.successor_turn_attempt_id = call.turn_attempt_id
              JOIN model_call AS predecessor
                ON predecessor.model_call_id = successor.predecessor_model_call_id
              JOIN turn_attempt AS attempt
                ON attempt.turn_attempt_id = call.turn_attempt_id
               AND attempt.turn_id = call.turn_id
               AND attempt.session_id = call.session_id
              JOIN turn_lifecycle AS lifecycle
                ON lifecycle.turn_id = call.turn_id
               AND lifecycle.session_id = call.session_id
             WHERE call.model_call_id = checked_model_call_id
               AND call.turn_id = predecessor.turn_id
               AND call.session_id = predecessor.session_id
               AND call.resolved_provider_model_identity_id =
                   predecessor.resolved_provider_model_identity_id
               AND ROW(
                    call.selection_kind,
                    call.direct_model_selection_id,
                    call.frozen_model_alias_id,
                    call.frozen_alias_selected_direct_id
               ) IS NOT DISTINCT FROM ROW(
                    predecessor.selection_kind,
                    predecessor.direct_model_selection_id,
                    predecessor.frozen_model_alias_id,
                    predecessor.frozen_alias_selected_direct_id
               )
               AND (
                    (
                        call.state_kind = 'prepared'
                        AND attempt.state_kind = 'prepared'
                        AND lifecycle.state_kind = 'active'
                        AND lifecycle.active_phase_kind = 'running'
                        AND lifecycle.current_attempt_id = call.turn_attempt_id
                    )
                    OR (
                        call.state_kind = 'in_flight'
                        AND attempt.state_kind = 'running'
                        AND lifecycle.state_kind = 'active'
                        AND lifecycle.active_phase_kind = 'running'
                        AND lifecycle.current_attempt_id = call.turn_attempt_id
                    )
                    -- An interrupt on a rotated in-flight call moves the call
                    -- to cancellation_requested and its attempt to
                    -- stop_requested together, exactly as the pre-pool
                    -- validator admits. Requiring a still-running attempt here
                    -- rejected the submit-input transaction, so a provider call
                    -- made by a substituted credential could not be cancelled.
                    OR (
                        call.state_kind = 'cancellation_requested'
                        AND attempt.state_kind IN ('running', 'stop_requested')
                        AND lifecycle.state_kind = 'active'
                        AND lifecycle.active_phase_kind = 'running'
                        AND lifecycle.current_attempt_id = call.turn_attempt_id
                    )
                    OR (
                        call.state_kind = 'terminal'
                        AND attempt.state_kind = 'ended'
                        AND (
                            lifecycle.state_kind = 'terminal'
                            OR EXISTS (
                                SELECT 1
                                  FROM credential_pool_availability_successor AS later
                                 WHERE later.predecessor_model_call_id = call.model_call_id
                            )
                        )
                    )
               )
        ) THEN
            RAISE EXCEPTION 'availability successor call lacks exact lifecycle state'
                USING ERRCODE = '23514';
        END IF;
        RETURN;
    END IF;

    PERFORM assert_model_call_final_state_before_credential_pools(
        checked_model_call_id
    );
END;
$$;

CREATE OR REPLACE FUNCTION assert_tool_round_final_state(checked_model_call_id uuid) RETURNS void
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

DROP FUNCTION claim_deferred_final_state_validation(text, uuid);
