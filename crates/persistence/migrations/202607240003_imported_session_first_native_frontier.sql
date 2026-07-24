-- Imported ancestry changes only the base prefix of a first native turn.
-- Every later lifecycle, attempt, model-call, terminal, and outbox check remains
-- delegated to the existing authoritative validators below.
CREATE FUNCTION first_native_starting_frontier_matches_seed(
    checked_session uuid,
    checked_starting_frontier uuid
)
RETURNS boolean
LANGUAGE plpgsql
STABLE
AS $$
DECLARE
    checked_ancestry text;
    starting_member_count numeric(20, 0);
    seed_frontier uuid;
    seed_member_count numeric(20, 0);
    actual_seed_member_count bigint;
BEGIN
    SELECT ancestry_kind
      INTO checked_ancestry
      FROM session
     WHERE session_id = checked_session;

    SELECT member_count
      INTO starting_member_count
      FROM context_frontier
     WHERE owning_session_id = checked_session
       AND context_frontier_id = checked_starting_frontier;

    IF checked_ancestry IS NULL OR starting_member_count IS NULL THEN
        RETURN false;
    END IF;

    IF checked_ancestry = 'none' THEN
        RETURN starting_member_count = 1;
    END IF;
    IF checked_ancestry <> 'imported_conversation' THEN
        RETURN false;
    END IF;

    SELECT seed.seed_context_frontier_id, frontier.member_count
      INTO seed_frontier, seed_member_count
      FROM imported_session_seed AS seed
      JOIN context_frontier AS frontier
        ON frontier.owning_session_id = seed.session_id
       AND frontier.context_frontier_id = seed.seed_context_frontier_id
     WHERE seed.session_id = checked_session;

    IF NOT FOUND
       OR seed_member_count IS NULL
       OR starting_member_count IS DISTINCT FROM seed_member_count + 1
    THEN
        RETURN false;
    END IF;

    SELECT count(*)
      INTO actual_seed_member_count
      FROM context_frontier_member
     WHERE owning_session_id = checked_session
       AND context_frontier_id = seed_frontier;
    IF actual_seed_member_count IS DISTINCT FROM seed_member_count THEN
        RETURN false;
    END IF;

    RETURN NOT EXISTS (
        SELECT 1
          FROM context_frontier_member AS seed_member
          LEFT JOIN context_frontier_member AS starting_member
            ON starting_member.owning_session_id = checked_session
           AND starting_member.context_frontier_id =
                   checked_starting_frontier
           AND starting_member.member_position = seed_member.member_position
           AND starting_member.source_session_id =
                   seed_member.source_session_id
           AND starting_member.semantic_entry_id =
                   seed_member.semantic_entry_id
         WHERE seed_member.owning_session_id = checked_session
           AND seed_member.context_frontier_id = seed_frontier
           AND starting_member.member_position IS NULL
    );
END;
$$;

CREATE OR REPLACE FUNCTION assert_turn_lifecycle_final_state_without_steering(
    checked_turn_id uuid
)
RETURNS void
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
      FROM semantic_transcript_entry
     WHERE source_session_id = checked_session_id
       AND payload_kind = 'origin_accepted_input'
       AND origin_accepted_input_id = checked_origin_input_id;

    SELECT semantic_entry_id
      INTO origin_entry_id
      FROM semantic_transcript_entry
     WHERE source_session_id = checked_session_id
       AND payload_kind = 'origin_accepted_input'
       AND origin_accepted_input_id = checked_origin_input_id;

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
        ) THEN
            RAISE EXCEPTION 'turn % has invalid first lineage', checked_turn_id
                USING ERRCODE = '23514';
        END IF;
    ELSE
        SELECT state_kind, acceptance_position, terminal_frontier_id
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

        IF predecessor_state IS DISTINCT FROM 'terminal'
           OR predecessor_position IS DISTINCT FROM expected_predecessor_position
        THEN
            RAISE EXCEPTION 'turn % does not follow its immediate terminal predecessor', checked_turn_id
                USING ERRCODE = '23514';
        END IF;

        SELECT member_count
          INTO predecessor_terminal_member_count
          FROM context_frontier
         WHERE owning_session_id = checked_session_id
           AND context_frontier_id = predecessor_terminal_frontier;

        SELECT count(*)
          INTO prefix_mismatch_count
          FROM context_frontier_member AS predecessor_member
          LEFT JOIN context_frontier_member AS starting_member
            ON starting_member.owning_session_id = checked_session_id
           AND starting_member.context_frontier_id = checked_starting_frontier
           AND starting_member.member_position = predecessor_member.member_position
           AND starting_member.source_session_id = predecessor_member.source_session_id
           AND starting_member.semantic_entry_id = predecessor_member.semantic_entry_id
         WHERE predecessor_member.owning_session_id = checked_session_id
           AND predecessor_member.context_frontier_id = predecessor_terminal_frontier
           AND starting_member.member_position IS NULL;

        IF starting_member_count IS DISTINCT FROM predecessor_terminal_member_count + 1
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

    SELECT count(*)
      INTO prefix_mismatch_count
      FROM context_frontier_member AS starting_member
      LEFT JOIN context_frontier_member AS terminal_member
        ON terminal_member.owning_session_id = checked_session_id
       AND terminal_member.context_frontier_id = checked_terminal_frontier
       AND terminal_member.member_position = starting_member.member_position
       AND terminal_member.source_session_id = starting_member.source_session_id
       AND terminal_member.semantic_entry_id = starting_member.semantic_entry_id
     WHERE starting_member.owning_session_id = checked_session_id
       AND starting_member.context_frontier_id = checked_starting_frontier
       AND terminal_member.member_position IS NULL;

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

CREATE OR REPLACE FUNCTION assert_terminal_started_turn_common_final_state(
    checked_turn_id uuid
)
RETURNS void
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
      FROM semantic_transcript_entry
     WHERE source_session_id = checked_session
       AND payload_kind = 'origin_accepted_input'
       AND origin_accepted_input_id = checked_origin_input;
    IF origin_entry_count <> 1 THEN
        RAISE EXCEPTION
            'terminal turn % lacks its exact origin entry',
            checked_turn_id
            USING ERRCODE = '23514';
    END IF;
    SELECT semantic_entry_id
      INTO origin_entry
      FROM semantic_transcript_entry
     WHERE source_session_id = checked_session
       AND payload_kind = 'origin_accepted_input'
       AND origin_accepted_input_id = checked_origin_input;

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
        SELECT terminal_frontier_id
          INTO predecessor_frontier
          FROM turn_lifecycle
         WHERE turn_id = checked_predecessor
           AND session_id = checked_session
           AND state_kind = 'terminal';
        IF NOT FOUND THEN
            RAISE EXCEPTION
                'terminal turn % predecessor is not terminal',
                checked_turn_id
                USING ERRCODE = '23514';
        END IF;
        SELECT member_count
          INTO predecessor_member_count
          FROM context_frontier
         WHERE owning_session_id = checked_session
           AND context_frontier_id = predecessor_frontier;
        SELECT count(*)
          INTO prefix_mismatch_count
          FROM context_frontier_member AS predecessor_member
          LEFT JOIN context_frontier_member AS starting_member
            ON starting_member.owning_session_id = checked_session
           AND starting_member.context_frontier_id = checked_starting_frontier
           AND starting_member.member_position = predecessor_member.member_position
           AND starting_member.source_session_id = predecessor_member.source_session_id
           AND starting_member.semantic_entry_id = predecessor_member.semantic_entry_id
         WHERE predecessor_member.owning_session_id = checked_session
           AND predecessor_member.context_frontier_id = predecessor_frontier
           AND starting_member.member_position IS NULL;
        IF starting_member_count
               IS DISTINCT FROM predecessor_member_count + 1
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
