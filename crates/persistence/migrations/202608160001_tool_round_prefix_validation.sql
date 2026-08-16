-- Resolve each recursive frontier once while validating a tool-round prefix.
-- Leaving both view references inline lets the planner execute the boundary
-- resolver once per source member, making deferred checks grow quadratically.
CREATE OR REPLACE FUNCTION assert_tool_round_final_state(
    checked_model_call_id uuid
)
RETURNS void
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
    WITH source_members AS MATERIALIZED (
        SELECT member_position, source_session_id, semantic_entry_id
          FROM context_frontier_member
         WHERE owning_session_id = round_record.session_id
           AND context_frontier_id = source_frontier
    ),
    boundary_members AS MATERIALIZED (
        SELECT member_position, source_session_id, semantic_entry_id
          FROM context_frontier_member
         WHERE owning_session_id = round_record.session_id
           AND context_frontier_id = round_record.boundary_frontier_id
    )
    SELECT count(*)
      INTO prefix_mismatch_count
      FROM (
            SELECT * FROM source_members
            EXCEPT
            SELECT * FROM boundary_members
      ) AS mismatch;

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
