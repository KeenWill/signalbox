-- Model calls: the model-call record and its identity, transition outbox
-- events, user overrides, credential-pool selection state, context compaction
-- and its model calls, and the context frontier with its deltas and the
-- resolved member view.

-- Function bodies may read tables a later section or file creates;
-- 202609010000_core.sql explains why validation is deferred.
SET check_function_bodies = false;

--
-- Functions.
--

--
-- Name: assert_context_frontier_complete_membership(uuid, uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION assert_context_frontier_complete_membership(checked_owning_session_id uuid, checked_context_frontier_id uuid) RETURNS void
    LANGUAGE plpgsql
    AS $$
DECLARE
    expected_count numeric(20, 0);
    actual_count numeric(20, 0);
    distinct_position_count numeric(20, 0);
    distinct_entry_count numeric(20, 0);
    first_position numeric(20, 0);
    last_position numeric(20, 0);
    cycle_found boolean;
BEGIN
    SELECT member_count
      INTO expected_count
      FROM context_frontier
     WHERE owning_session_id = checked_owning_session_id
       AND context_frontier_id = checked_context_frontier_id;

    IF NOT FOUND THEN
        RETURN;
    END IF;

    WITH RECURSIVE prefix_chain (
        context_frontier_id,
        prefix_context_frontier_id,
        visited,
        cycle
    ) AS (
        SELECT
            frontier.context_frontier_id,
            frontier.prefix_context_frontier_id,
            ARRAY[frontier.context_frontier_id],
            false
          FROM context_frontier AS frontier
         WHERE frontier.owning_session_id =
                   checked_owning_session_id
           AND frontier.context_frontier_id =
                   checked_context_frontier_id
        UNION ALL
        SELECT
            prefix.context_frontier_id,
            prefix.prefix_context_frontier_id,
            prefix_chain.visited || prefix.context_frontier_id,
            prefix.context_frontier_id =
                ANY(prefix_chain.visited)
          FROM prefix_chain
          JOIN context_frontier AS prefix
            ON prefix.owning_session_id =
                   checked_owning_session_id
           AND prefix.context_frontier_id =
                   prefix_chain.prefix_context_frontier_id
         WHERE NOT prefix_chain.cycle
    )
    SELECT COALESCE(bool_or(cycle), false)
      INTO cycle_found
      FROM prefix_chain;

    SELECT
        count(*)::numeric(20, 0),
        count(DISTINCT member_position)::numeric(20, 0),
        count(DISTINCT (source_session_id, semantic_entry_id))::numeric(20, 0),
        min(member_position),
        max(member_position)
      INTO
        actual_count,
        distinct_position_count,
        distinct_entry_count,
        first_position,
        last_position
      FROM resolve_context_frontier_members(
        checked_owning_session_id,
        checked_context_frontier_id
      );

    IF cycle_found
       OR actual_count <> expected_count
       OR distinct_position_count <> expected_count
       OR distinct_entry_count <> expected_count
       OR (
           expected_count > 0
           AND (
               first_position <> 1
               OR last_position <> expected_count
           )
       )
    THEN
        RAISE EXCEPTION
            'context frontier (%, %) does not have complete contiguous distinct membership',
            checked_owning_session_id,
            checked_context_frontier_id
            USING ERRCODE = '23514';
    END IF;
END;
$$;


SET default_tablespace = '';

SET default_table_access_method = heap;

--
-- Name: assert_failed_terminal_execution_before_credential_pools(uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION assert_failed_terminal_execution_before_credential_pools(checked_turn_id uuid) RETURNS void
    LANGUAGE plpgsql
    AS $$
DECLARE
    checked_session uuid;
    checked_attempt uuid;
    checked_call uuid;
    checked_attempt_history boolean;
    cancellation_failure boolean;
    attempt_count bigint;
    call_count bigint;
BEGIN
    SELECT
        lifecycle.session_id,
        lifecycle.terminal_attempt_id,
        lifecycle.terminal_model_call_id,
        lifecycle.attempt_history_present,
        EXISTS (
            SELECT 1
              FROM turn_attempt AS attempt
             WHERE attempt.turn_attempt_id = lifecycle.terminal_attempt_id
               AND attempt.turn_id = lifecycle.turn_id
               AND attempt.session_id = lifecycle.session_id
               AND attempt.end_variant = 'after_cancellation'
               AND attempt.end_disposition = 'known_failure'
        )
      INTO
        checked_session,
        checked_attempt,
        checked_call,
        checked_attempt_history,
        cancellation_failure
      FROM turn_lifecycle AS lifecycle
     WHERE lifecycle.turn_id = checked_turn_id
       AND lifecycle.state_kind = 'terminal'
       AND lifecycle.terminal_disposition_kind = 'failed';

    IF NOT FOUND OR NOT cancellation_failure THEN
        PERFORM assert_failed_terminal_execution_without_cancellation(
            checked_turn_id
        );
        RETURN;
    END IF;

    SELECT count(*)
      INTO attempt_count
      FROM turn_attempt
     WHERE turn_id = checked_turn_id
       AND session_id = checked_session;
    SELECT count(*)
      INTO call_count
      FROM model_call
     WHERE turn_id = checked_turn_id
       AND session_id = checked_session;
    IF checked_attempt_history IS DISTINCT FROM true
       OR attempt_count <> 1
       OR checked_attempt IS NULL
    THEN
        RAISE EXCEPTION
            'post-cancellation failure lacks its exact single attempt'
            USING ERRCODE = '23514';
    END IF;
    IF (call_count = 0 AND checked_call IS NOT NULL)
       OR call_count > 1
       OR (call_count = 1 AND checked_call IS NULL)
    THEN
        RAISE EXCEPTION
            'post-cancellation failure has inconsistent call provenance'
            USING ERRCODE = '23514';
    END IF;

    PERFORM assert_interrupt_attempt_proof(checked_attempt);
    IF checked_call IS NOT NULL THEN
        IF NOT EXISTS (
            SELECT 1
              FROM model_call
             WHERE model_call_id = checked_call
               AND turn_attempt_id = checked_attempt
               AND turn_id = checked_turn_id
               AND session_id = checked_session
               AND state_kind = 'terminal'
               AND terminal_disposition_kind = 'known_failed'
        ) THEN
            RAISE EXCEPTION
                'post-cancellation failure lacks its exact terminal call'
                USING ERRCODE = '23514';
        END IF;
        PERFORM assert_model_call_final_state(checked_call);
    END IF;
END;
$$;


--
-- Name: assert_model_call_final_state(uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION assert_model_call_final_state(checked_model_call_id uuid) RETURNS void
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
    IF NOT claim_deferred_final_state_validation('model_call', checked_model_call_id) THEN
        RETURN;
    END IF;

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


--
-- Name: assert_model_call_final_state_before_credential_pools(uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION assert_model_call_final_state_before_credential_pools(checked_model_call_id uuid) RETURNS void
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF EXISTS (
        SELECT 1
          FROM tool_round
         WHERE producing_model_call_id = checked_model_call_id
    ) THEN
        PERFORM assert_tool_round_final_state(checked_model_call_id);
    ELSE
        PERFORM assert_model_call_final_state_without_tool_round(
            checked_model_call_id
        );
    END IF;
END;
$$;


--
-- Name: assert_model_call_final_state_without_stop(uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION assert_model_call_final_state_without_stop(checked_model_call_id uuid) RETURNS void
    LANGUAGE plpgsql
    AS $$
DECLARE
    checked_turn_id uuid;
    checked_session_id uuid;
    checked_attempt_id uuid;
    checked_selection_kind text;
    checked_direct_id uuid;
    checked_alias_id uuid;
    checked_alias_selected_id uuid;
    checked_target_id uuid;
    checked_frontier_id uuid;
    checked_state text;
    checked_disposition text;
    origin_frozen_kind text;
    origin_direct_id uuid;
    origin_alias_id uuid;
    origin_alias_selected_id uuid;
    pinned_target_id uuid;
    attempt_state text;
    attempt_disposition text;
    turn_state text;
    active_phase text;
    current_attempt uuid;
    recovery_call uuid;
    terminal_attempt uuid;
    terminal_call uuid;
    terminal_disposition text;
    starting_frontier uuid;
BEGIN
    SELECT
        turn_id,
        session_id,
        turn_attempt_id,
        selection_kind,
        direct_model_selection_id,
        frozen_model_alias_id,
        frozen_alias_selected_direct_id,
        resolved_provider_model_identity_id,
        context_frontier_id,
        state_kind,
        terminal_disposition_kind
      INTO
        checked_turn_id,
        checked_session_id,
        checked_attempt_id,
        checked_selection_kind,
        checked_direct_id,
        checked_alias_id,
        checked_alias_selected_id,
        checked_target_id,
        checked_frontier_id,
        checked_state,
        checked_disposition
      FROM model_call
     WHERE model_call_id = checked_model_call_id;

    IF NOT FOUND THEN
        RETURN;
    END IF;

    SELECT
        origin.frozen_model_kind,
        origin.frozen_direct_model_selection_id,
        origin.frozen_model_alias_id,
        origin.frozen_alias_selected_direct_id,
        lifecycle.pinned_provider_model_identity_id,
        lifecycle.state_kind,
        lifecycle.active_phase_kind,
        lifecycle.current_attempt_id,
        lifecycle.recovery_model_call_id,
        lifecycle.terminal_attempt_id,
        lifecycle.terminal_model_call_id,
        lifecycle.terminal_disposition_kind,
        lifecycle.starting_frontier_id
      INTO
        origin_frozen_kind,
        origin_direct_id,
        origin_alias_id,
        origin_alias_selected_id,
        pinned_target_id,
        turn_state,
        active_phase,
        current_attempt,
        recovery_call,
        terminal_attempt,
        terminal_call,
        terminal_disposition,
        starting_frontier
      FROM turn_lifecycle AS lifecycle
      JOIN LATERAL turn_origin_exact_model_configuration(
            lifecycle.turn_id,
            lifecycle.session_id
      ) AS origin ON TRUE
     WHERE lifecycle.turn_id = checked_turn_id
       AND lifecycle.session_id = checked_session_id;

    IF NOT FOUND THEN
        RAISE EXCEPTION 'model call requires its exact owning turn'
            USING ERRCODE = '23503';
    END IF;

    IF ROW(
        checked_selection_kind,
        checked_direct_id,
        checked_alias_id,
        checked_alias_selected_id
    ) IS DISTINCT FROM ROW(
        origin_frozen_kind,
        origin_direct_id,
        origin_alias_id,
        origin_alias_selected_id
    ) THEN
        RAISE EXCEPTION 'model call selection differs from its frozen turn selection'
            USING ERRCODE = '23514';
    END IF;

    IF pinned_target_id IS DISTINCT FROM checked_target_id THEN
        RAISE EXCEPTION 'model call target differs from its independent turn-level pin'
            USING ERRCODE = '23514';
    END IF;

    PERFORM assert_model_call_steering_final_state(checked_model_call_id);

    SELECT state_kind, end_disposition
      INTO attempt_state, attempt_disposition
      FROM turn_attempt
     WHERE turn_attempt_id = checked_attempt_id
       AND turn_id = checked_turn_id
       AND session_id = checked_session_id;

    IF NOT FOUND THEN
        RAISE EXCEPTION 'model call requires its exact owning attempt'
            USING ERRCODE = '23503';
    END IF;

    IF checked_state = 'prepared' THEN
        IF turn_state IS DISTINCT FROM 'active'
           OR active_phase IS DISTINCT FROM 'running'
           OR current_attempt IS DISTINCT FROM checked_attempt_id
           
OR (
                attempt_state IS DISTINCT FROM 'prepared'
                AND NOT (
                    attempt_state = 'running'
                    AND continuation_frontier_closes_predecessor_tool_round(
                        checked_attempt_id,
                        checked_turn_id,
                        checked_session_id,
                        checked_frontier_id
                    )
                )
            )

        THEN
            RAISE EXCEPTION 'Prepared model call is not paired with its prepared attempt'
                USING ERRCODE = '23514';
        END IF;
    ELSIF checked_state IN ('in_flight', 'cancellation_requested') THEN
        IF turn_state IS DISTINCT FROM 'active'
           OR active_phase IS DISTINCT FROM 'running'
           OR current_attempt IS DISTINCT FROM checked_attempt_id
           OR attempt_state IS DISTINCT FROM 'running'
        THEN
            RAISE EXCEPTION 'issued model call is not paired with its running attempt'
                USING ERRCODE = '23514';
        END IF;
    ELSIF checked_disposition = 'ambiguous' THEN
        IF turn_state IS DISTINCT FROM 'active'
           OR active_phase IS DISTINCT FROM 'awaiting_model_call_recovery'
           OR current_attempt IS DISTINCT FROM checked_attempt_id
           OR recovery_call IS DISTINCT FROM checked_model_call_id
           OR attempt_state IS DISTINCT FROM 'ended'
           OR attempt_disposition NOT IN ('ambiguous', 'lost')
        THEN
            RAISE EXCEPTION 'Ambiguous model call lacks its exact durable recovery wait'
                USING ERRCODE = '23514';
        END IF;
    ELSIF checked_disposition = 'completed' THEN
        IF turn_state IS DISTINCT FROM 'terminal'
           OR terminal_disposition IS DISTINCT FROM 'completed'
           OR terminal_attempt IS DISTINCT FROM checked_attempt_id
           OR terminal_call IS DISTINCT FROM checked_model_call_id
           OR attempt_state IS DISTINCT FROM 'ended'
           OR (
                attempt_disposition IS DISTINCT FROM 'turn_completed'
                AND attempt_disposition IS DISTINCT FROM 'lost'
           )
        THEN
            RAISE EXCEPTION 'Completed model call lacks its exact terminal turn outcome'
                USING ERRCODE = '23514';
        END IF;
    ELSIF checked_disposition = 'refused' THEN
        IF turn_state IS DISTINCT FROM 'terminal'
           OR terminal_disposition IS DISTINCT FROM 'refused'
           OR terminal_attempt IS DISTINCT FROM checked_attempt_id
           OR terminal_call IS DISTINCT FROM checked_model_call_id
           OR attempt_state IS DISTINCT FROM 'ended'
           OR (
                attempt_disposition IS DISTINCT FROM 'turn_refused'
                AND attempt_disposition IS DISTINCT FROM 'lost'
           )
        THEN
            RAISE EXCEPTION 'Refused model call lacks its exact terminal turn outcome'
                USING ERRCODE = '23514';
        END IF;
    ELSE
        IF turn_state IS DISTINCT FROM 'terminal'
           OR terminal_disposition IS DISTINCT FROM 'failed'
           OR attempt_state IS DISTINCT FROM 'ended'
           OR attempt_disposition NOT IN ('known_failure', 'lost')
        THEN
            RAISE EXCEPTION 'failed physical call lacks its exact failed turn outcome'
                USING ERRCODE = '23514';
        END IF;
    END IF;
END;
$$;


--
-- Name: assert_model_call_final_state_without_tool_round(uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION assert_model_call_final_state_without_tool_round(checked_model_call_id uuid) RETURNS void
    LANGUAGE plpgsql
    AS $$
DECLARE
    stopped_state boolean;
BEGIN
    SELECT EXISTS (
        SELECT 1
          FROM model_call AS call
          JOIN turn_attempt AS attempt
            ON attempt.turn_attempt_id = call.turn_attempt_id
           AND attempt.turn_id = call.turn_id
           AND attempt.session_id = call.session_id
          JOIN turn_lifecycle AS lifecycle
            ON lifecycle.turn_id = call.turn_id
           AND lifecycle.session_id = call.session_id
         WHERE call.model_call_id = checked_model_call_id
           AND (
                (
                    attempt.interrupt_command_id IS NOT NULL
                    AND call.state_kind = 'cancellation_requested'
                )
                OR (
                    call.state_kind = 'terminal'
                    AND (
                        (
                            call.terminal_disposition_kind = 'cancelled'
                            AND attempt.interrupt_command_id IS NOT NULL
                            AND attempt.end_disposition = 'cancelled'
                        )
                        OR (
                            call.terminal_disposition_kind = 'ambiguous'
                            AND attempt.end_disposition IN ('ambiguous', 'lost')
                            AND lifecycle.state_kind = 'terminal'
                            AND lifecycle.terminal_disposition_kind
                                = 'reconciliation_required'
                            AND lifecycle.terminal_model_call_id
                                = checked_model_call_id
                        )
                    )
                )
           )
    )
      INTO stopped_state;

    IF stopped_state THEN
        PERFORM assert_stopped_model_call_final_state(checked_model_call_id);
    ELSE
        PERFORM assert_model_call_final_state_without_stop(
            checked_model_call_id
        );
    END IF;
END;
$$;


--
-- Name: assert_model_call_steering_final_state(uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION assert_model_call_steering_final_state(checked_model_call_id uuid) RETURNS void
    LANGUAGE plpgsql
    AS $$
DECLARE
    checked_session uuid;
    checked_turn uuid;
    predecessor_attempt uuid;
    checked_frontier uuid;
    starting_frontier uuid;
    starting_count numeric(20, 0);
    checked_count numeric(20, 0);
    result_boundary uuid;
    result_producing_call uuid;
    result_boundary_count numeric(20, 0);
    result_request_count bigint;
    suffix_start_count numeric(20, 0);
    suffix_count bigint;
    consumed_count bigint;
    malformed_result_count bigint;
    malformed_count bigint;
BEGIN
    SELECT
        call.session_id,
        call.turn_id,
        attempt.continued_from_attempt_id,
        call.context_frontier_id,
        lifecycle.starting_frontier_id
      INTO
        checked_session,
        checked_turn,
        predecessor_attempt,
        checked_frontier,
        starting_frontier
      FROM model_call AS call
      JOIN turn_attempt AS attempt
        ON attempt.turn_attempt_id = call.turn_attempt_id
       AND attempt.turn_id = call.turn_id
       AND attempt.session_id = call.session_id
      JOIN turn_lifecycle AS lifecycle
        ON lifecycle.turn_id = call.turn_id
       AND lifecycle.session_id = call.session_id
     WHERE call.model_call_id = checked_model_call_id;

    IF NOT FOUND THEN
        RETURN;
    END IF;

    SELECT member_count
      INTO starting_count
      FROM context_frontier
     WHERE owning_session_id = checked_session
       AND context_frontier_id = starting_frontier;
    SELECT member_count
      INTO checked_count
      FROM context_frontier
     WHERE owning_session_id = checked_session
       AND context_frontier_id = checked_frontier;

    IF starting_count IS NULL
       OR checked_count IS NULL
       OR checked_count < starting_count
       OR (
            checked_frontier <> starting_frontier
            AND checked_count = starting_count
       )
       OR NOT context_frontier_preserves_prefix(
            checked_session,
            starting_frontier,
            checked_frontier
       )
    THEN
        RAISE EXCEPTION
            'model call frontier does not preserve its turn-start prefix'
            USING ERRCODE = '23514';
    END IF;

    suffix_start_count := starting_count;
    IF predecessor_attempt IS NOT NULL THEN
        SELECT
            round.boundary_frontier_id,
            producing_call.model_call_id,
            boundary.member_count,
            round.request_count
          INTO
            result_boundary,
            result_producing_call,
            result_boundary_count,
            result_request_count
          FROM model_call AS producing_call
          JOIN tool_round AS round
            ON round.producing_model_call_id = producing_call.model_call_id
           AND round.turn_id = producing_call.turn_id
           AND round.session_id = producing_call.session_id
          JOIN context_frontier AS boundary
            ON boundary.owning_session_id = round.session_id
           AND boundary.context_frontier_id = round.boundary_frontier_id
         WHERE producing_call.turn_attempt_id = predecessor_attempt
           AND producing_call.turn_id = checked_turn
           AND producing_call.session_id = checked_session
           AND producing_call.state_kind = 'terminal'
           AND producing_call.terminal_disposition_kind = 'completed'
           AND round.boundary_kind = 'continuing';

        IF NOT FOUND THEN
            RAISE EXCEPTION
                'continued model call lacks its predecessor tool round'
                USING ERRCODE = '23514';
        END IF;

        IF checked_count < result_boundary_count + result_request_count
           OR NOT context_frontier_preserves_prefix(
                checked_session,
                result_boundary,
                checked_frontier
           )
        THEN
            RAISE EXCEPTION
                'continued model call omits its tool-round boundary'
                USING ERRCODE = '23514';
        END IF;

        SELECT count(*)
          INTO malformed_result_count
          FROM generate_series(
                0,
                result_request_count - 1
          ) AS expected(request_ordinal)
          JOIN tool_request AS request
            ON request.producing_model_call_id = result_producing_call
           AND request.request_ordinal = expected.request_ordinal
          LEFT JOIN context_frontier_member AS member
            ON member.owning_session_id = checked_session
           AND member.context_frontier_id = checked_frontier
           AND member.member_position =
               result_boundary_count + expected.request_ordinal + 1
          LEFT JOIN semantic_transcript_entry AS entry
            ON entry.source_session_id = member.source_session_id
           AND entry.semantic_entry_id = member.semantic_entry_id
          LEFT JOIN tool_attempt AS attempt
            ON attempt.attempt_id = entry.tool_result_attempt_id
         WHERE member.source_session_id IS DISTINCT FROM checked_session
            OR (
                (
                    entry.payload_kind = 'tool_execution_result'
                    AND attempt.request_id = request.request_id
                )
                OR (
                    entry.payload_kind IN ('tool_denied', 'delegation_result')
                    AND entry.tool_result_request_id = request.request_id
                )
            ) IS NOT TRUE;

        IF malformed_result_count <> 0 THEN
            RAISE EXCEPTION
                'continued model call lacks proposal-ordered tool results'
                USING ERRCODE = '23514';
        END IF;
        suffix_start_count :=
            result_boundary_count + result_request_count;
    END IF;

    SELECT count(*)
      INTO suffix_count
      FROM context_frontier_member
     WHERE owning_session_id = checked_session
       AND context_frontier_id = checked_frontier
       AND member_position > suffix_start_count;

    SELECT count(*)
      INTO consumed_count
      FROM accepted_input
     WHERE session_id = checked_session
       AND expected_active_turn_id = checked_turn
       AND disposition_kind = 'consumed_as_steering'
       AND consuming_model_call_id = checked_model_call_id;

    SELECT consumed_count + count(*)
      INTO consumed_count
      FROM session_pending_delivery AS pending
     WHERE pending.recipient_session_id = checked_session
       AND NOT EXISTS (
            SELECT 1
              FROM context_frontier_member AS prior_member
             WHERE prior_member.owning_session_id = checked_session
               AND prior_member.context_frontier_id = checked_frontier
               AND prior_member.member_position <= suffix_start_count
               AND prior_member.source_session_id = checked_session
               AND prior_member.semantic_entry_id =
                    delegation_delivery_semantic_entry(
                        pending.recipient_session_id,
                        pending.delivery_sequence
                    )
       );

    SELECT count(*)
      INTO malformed_count
      FROM context_frontier_member AS member
      LEFT JOIN semantic_transcript_entry AS entry
        ON entry.source_session_id = member.source_session_id
       AND entry.semantic_entry_id = member.semantic_entry_id
      LEFT JOIN accepted_input AS accepted
        ON accepted.accepted_input_id = entry.origin_accepted_input_id
       AND accepted.session_id = entry.source_session_id
     WHERE member.owning_session_id = checked_session
       AND member.context_frontier_id = checked_frontier
       AND member.member_position > suffix_start_count
       AND NOT model_call_suffix_entry_is_valid(
            checked_session,
            checked_turn,
            checked_model_call_id,
            entry.source_session_id,
            entry.semantic_entry_id
       );

    IF suffix_count IS DISTINCT FROM consumed_count
       OR malformed_count <> 0
       OR NOT model_call_delivery_suffix_is_ordered(
            checked_session,
            checked_frontier,
            suffix_start_count
       )
       OR EXISTS (
            SELECT 1
              FROM accepted_input AS earlier
              JOIN accepted_input AS consumed
                ON consumed.session_id = earlier.session_id
               AND consumed.expected_active_turn_id =
                   earlier.expected_active_turn_id
               AND consumed.disposition_kind = 'consumed_as_steering'
               AND consumed.consuming_model_call_id =
                   checked_model_call_id
               AND consumed.acceptance_position >
                   earlier.acceptance_position
             WHERE earlier.session_id = checked_session
               AND earlier.expected_active_turn_id = checked_turn
               AND earlier.disposition_kind IN (
                    'pending_steering',
                    'reclassified_as_turn_origin'
               )
       )
       OR EXISTS (
            SELECT 1
              FROM (
                    SELECT
                        accepted.acceptance_position,
                        row_number() OVER (
                            ORDER BY accepted.acceptance_position
                        ) AS acceptance_order,
                        row_number() OVER (
                            ORDER BY member.member_position
                        ) AS member_order
                      FROM context_frontier_member AS member
                      JOIN semantic_transcript_entry AS entry
                        ON entry.source_session_id =
                           member.source_session_id
                       AND entry.semantic_entry_id =
                           member.semantic_entry_id
                      JOIN accepted_input AS accepted
                        ON accepted.accepted_input_id =
                           entry.origin_accepted_input_id
                       AND accepted.session_id = entry.source_session_id
                     WHERE member.owning_session_id = checked_session
                       AND member.context_frontier_id = checked_frontier
                       AND member.member_position > suffix_start_count
              ) AS ordered
             WHERE ordered.acceptance_order <> ordered.member_order
       )
    THEN
        RAISE EXCEPTION
            'model call steering suffix is not the exact accepted order'
            USING ERRCODE = '23514';
    END IF;
END;
$$;


--
-- Name: assert_stopped_model_call_final_state(uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION assert_stopped_model_call_final_state(checked_model_call_id uuid) RETURNS void
    LANGUAGE plpgsql
    AS $$
DECLARE
    checked_turn uuid;
    checked_session uuid;
    checked_attempt uuid;
    checked_state text;
    checked_disposition text;
    attempt_state text;
    attempt_variant text;
    attempt_disposition text;
    attempt_interrupt_command uuid;
    turn_state text;
    active_phase text;
    current_attempt uuid;
    terminal_attempt uuid;
    terminal_call uuid;
    terminal_disposition text;
BEGIN
    SELECT
        call.turn_id,
        call.session_id,
        call.turn_attempt_id,
        call.state_kind,
        call.terminal_disposition_kind,
        attempt.state_kind,
        attempt.end_variant,
        attempt.end_disposition,
        attempt.interrupt_command_id,
        lifecycle.state_kind,
        lifecycle.active_phase_kind,
        lifecycle.current_attempt_id,
        lifecycle.terminal_attempt_id,
        lifecycle.terminal_model_call_id,
        lifecycle.terminal_disposition_kind
      INTO
        checked_turn,
        checked_session,
        checked_attempt,
        checked_state,
        checked_disposition,
        attempt_state,
        attempt_variant,
        attempt_disposition,
        attempt_interrupt_command,
        turn_state,
        active_phase,
        current_attempt,
        terminal_attempt,
        terminal_call,
        terminal_disposition
      FROM model_call AS call
      JOIN turn_attempt AS attempt
        ON attempt.turn_attempt_id = call.turn_attempt_id
       AND attempt.turn_id = call.turn_id
       AND attempt.session_id = call.session_id
      JOIN turn_lifecycle AS lifecycle
        ON lifecycle.turn_id = call.turn_id
       AND lifecycle.session_id = call.session_id
     WHERE call.model_call_id = checked_model_call_id;

    IF NOT FOUND THEN
        RAISE EXCEPTION 'stopped model call lacks its exact attempt and turn'
            USING ERRCODE = '23503';
    END IF;

    IF attempt_interrupt_command IS NOT NULL THEN
        PERFORM assert_interrupt_attempt_proof(checked_attempt);
    END IF;
    PERFORM assert_model_call_steering_final_state(checked_model_call_id);

    IF checked_state = 'cancellation_requested' THEN
        IF turn_state IS DISTINCT FROM 'active'
           OR active_phase IS DISTINCT FROM 'running'
           OR current_attempt IS DISTINCT FROM checked_attempt
           OR attempt_state IS DISTINCT FROM 'stop_requested'
           OR attempt_variant IS NOT NULL
           OR attempt_disposition IS NOT NULL
        THEN
            RAISE EXCEPTION
                'cancellation-requested call lacks its durable stop request'
                USING ERRCODE = '23514';
        END IF;
    ELSIF checked_state = 'terminal'
          AND checked_disposition = 'cancelled'
          AND attempt_disposition = 'cancelled'
    THEN
        IF turn_state IS DISTINCT FROM 'terminal'
           OR terminal_disposition IS DISTINCT FROM 'cancelled'
           OR terminal_attempt IS DISTINCT FROM checked_attempt
           OR terminal_call IS DISTINCT FROM checked_model_call_id
           OR attempt_state IS DISTINCT FROM 'ended'
           OR attempt_variant IS DISTINCT FROM 'after_cancellation'
        THEN
            RAISE EXCEPTION
                'cancelled call lacks its exact cancelled turn outcome'
                USING ERRCODE = '23514';
        END IF;
    ELSIF checked_state = 'terminal'
          AND checked_disposition = 'ambiguous'
          AND attempt_disposition IN ('ambiguous', 'lost')
    THEN
        IF turn_state IS DISTINCT FROM 'terminal'
           OR terminal_disposition IS DISTINCT FROM 'reconciliation_required'
           OR terminal_attempt IS DISTINCT FROM checked_attempt
           OR terminal_call IS DISTINCT FROM checked_model_call_id
           OR attempt_state IS DISTINCT FROM 'ended'
           OR attempt_variant NOT IN ('without_stop', 'after_cancellation')
           OR (
                attempt_variant = 'without_stop'
                AND attempt_interrupt_command IS NOT NULL
           )
           OR (
                attempt_variant = 'after_cancellation'
                AND attempt_interrupt_command IS NULL
           )
        THEN
            RAISE EXCEPTION
                'ambiguous stopped call lacks exact reconciliation outcome'
                USING ERRCODE = '23514';
        END IF;
    ELSE
        RAISE EXCEPTION 'unsupported stopped model-call state'
            USING ERRCODE = '23514';
    END IF;
END;
$$;


--
-- Name: context_frontier_member_position(uuid, uuid, uuid, uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION context_frontier_member_position(checked_session_id uuid, checked_frontier_id uuid, checked_source_session_id uuid, checked_semantic_entry_id uuid) RETURNS numeric
    LANGUAGE sql STABLE
    AS $$
    WITH RECURSIVE checked_chain AS MATERIALIZED (
        SELECT
            frontier.context_frontier_id,
            frontier.prefix_context_frontier_id
          FROM context_frontier AS frontier
         WHERE frontier.owning_session_id = checked_session_id
           AND frontier.context_frontier_id = checked_frontier_id
        UNION ALL
        SELECT
            prefix.context_frontier_id,
            prefix.prefix_context_frontier_id
          FROM checked_chain AS chain
          JOIN context_frontier AS prefix
            ON prefix.owning_session_id = checked_session_id
           AND prefix.context_frontier_id =
                   chain.prefix_context_frontier_id
         WHERE NOT EXISTS (
                SELECT 1
                  FROM context_frontier_delta AS found
                 WHERE found.owning_session_id = checked_session_id
                   AND found.context_frontier_id = chain.context_frontier_id
                   AND found.source_session_id = checked_source_session_id
                   AND found.semantic_entry_id = checked_semantic_entry_id
         )
    )
    SELECT member.member_position
      FROM checked_chain AS chain
      JOIN context_frontier_delta AS member
        ON member.owning_session_id = checked_session_id
       AND member.context_frontier_id = chain.context_frontier_id
       AND member.source_session_id = checked_source_session_id
       AND member.semantic_entry_id = checked_semantic_entry_id
     LIMIT 1
$$;


--
-- Name: context_frontier_preserves_prefix(uuid, uuid, uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION context_frontier_preserves_prefix(checked_session_id uuid, prefix_frontier_id uuid, checked_frontier_id uuid) RETURNS boolean
    LANGUAGE sql STABLE
    AS $$
    WITH RECURSIVE checked_chain AS MATERIALIZED (
        SELECT
            frontier.context_frontier_id,
            frontier.prefix_context_frontier_id
          FROM context_frontier AS frontier
         WHERE frontier.owning_session_id = checked_session_id
           AND frontier.context_frontier_id = checked_frontier_id
        UNION
        SELECT
            prefix.context_frontier_id,
            prefix.prefix_context_frontier_id
          FROM checked_chain AS chain
          JOIN context_frontier AS prefix
            ON prefix.owning_session_id = checked_session_id
           AND prefix.context_frontier_id = chain.prefix_context_frontier_id
         WHERE chain.context_frontier_id <> prefix_frontier_id
    ),
    prefix_members AS MATERIALIZED (
        SELECT member_position, source_session_id, semantic_entry_id
          FROM context_frontier_member
         WHERE owning_session_id = checked_session_id
           AND context_frontier_id = prefix_frontier_id
    ),
    checked_members AS MATERIALIZED (
        SELECT member_position, source_session_id, semantic_entry_id
          FROM context_frontier_member
         WHERE owning_session_id = checked_session_id
           AND context_frontier_id = checked_frontier_id
    )
    SELECT CASE
        WHEN EXISTS (
            SELECT 1 FROM checked_chain
             WHERE context_frontier_id = prefix_frontier_id
        ) THEN true
        ELSE NOT EXISTS (
            SELECT 1
              FROM prefix_members AS prefix
              LEFT JOIN checked_members AS checked
                ON checked.member_position = prefix.member_position
             WHERE ROW(checked.source_session_id, checked.semantic_entry_id)
                   IS DISTINCT FROM
                   ROW(prefix.source_session_id, prefix.semantic_entry_id)
        )
    END
$$;


--
-- Name: continuation_frontier_closes_predecessor_tool_round(uuid, uuid, uuid, uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION continuation_frontier_closes_predecessor_tool_round(checked_attempt_id uuid, checked_turn_id uuid, checked_session_id uuid, checked_frontier_id uuid) RETURNS boolean
    LANGUAGE sql STABLE
    AS $$
    SELECT EXISTS (
        SELECT 1
          FROM turn_attempt AS continuation_attempt
          JOIN model_call AS predecessor_call
            ON predecessor_call.turn_attempt_id =
               continuation_attempt.continued_from_attempt_id
           AND predecessor_call.turn_id = continuation_attempt.turn_id
           AND predecessor_call.session_id = continuation_attempt.session_id
           AND predecessor_call.state_kind = 'terminal'
           AND predecessor_call.terminal_disposition_kind = 'completed'
          JOIN tool_round AS predecessor_round
            ON predecessor_round.producing_model_call_id =
               predecessor_call.model_call_id
           AND predecessor_round.turn_id = predecessor_call.turn_id
           AND predecessor_round.session_id = predecessor_call.session_id
           AND predecessor_round.boundary_kind = 'continuing'
          JOIN context_frontier AS boundary
            ON boundary.owning_session_id = predecessor_round.session_id
           AND boundary.context_frontier_id =
               predecessor_round.boundary_frontier_id
         WHERE continuation_attempt.turn_attempt_id = checked_attempt_id
           AND continuation_attempt.turn_id = checked_turn_id
           AND continuation_attempt.session_id = checked_session_id
           AND continuation_attempt.continued_from_attempt_id IS NOT NULL
           AND context_frontier_preserves_prefix(
                predecessor_round.session_id,
                predecessor_round.boundary_frontier_id,
                checked_frontier_id
           )
           AND NOT EXISTS (
                SELECT 1
                  FROM generate_series(
                        0,
                        predecessor_round.request_count::bigint - 1
                  ) AS expected(request_ordinal)
                  JOIN tool_request AS request
                    ON request.producing_model_call_id =
                       predecessor_round.producing_model_call_id
                   AND request.request_ordinal =
                       expected.request_ordinal
                  LEFT JOIN context_frontier_member AS result_member
                    ON result_member.owning_session_id =
                       predecessor_round.session_id
                   AND result_member.context_frontier_id =
                       checked_frontier_id
                   AND result_member.member_position =
                       boundary.member_count + expected.request_ordinal + 1
                  LEFT JOIN semantic_transcript_entry AS result_entry
                    ON result_entry.source_session_id =
                       result_member.source_session_id
                   AND result_entry.semantic_entry_id =
                       result_member.semantic_entry_id
                   AND result_entry.payload_kind IN (
                       'tool_execution_result', 'tool_denied', 'tool_closed_by_turn_end', 'delegation_result'
                   )
                  LEFT JOIN tool_attempt AS result_attempt
                    ON result_attempt.attempt_id =
                       result_entry.tool_result_attempt_id
                 WHERE result_member.member_position IS NULL
                    OR result_entry.semantic_entry_id IS NULL
                    OR (
                        result_entry.tool_result_request_id
                            IS DISTINCT FROM request.request_id
                        AND result_attempt.request_id
                            IS DISTINCT FROM request.request_id
                    )
           )
    );
$$;


--
-- Name: first_native_starting_frontier_matches_seed(uuid, uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION first_native_starting_frontier_matches_seed(checked_session uuid, checked_starting_frontier uuid) RETURNS boolean
    LANGUAGE plpgsql STABLE
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


--
-- Name: model_call_delivery_suffix_is_ordered(uuid, uuid, numeric); Type: FUNCTION; Schema: public
--

CREATE FUNCTION model_call_delivery_suffix_is_ordered(checked_session uuid, checked_frontier uuid, suffix_start numeric) RETURNS boolean
    LANGUAGE sql STABLE
    AS $$
    SELECT NOT EXISTS (
        SELECT 1
          FROM (
                SELECT
                    pending.delivery_sequence,
                    row_number() OVER (
                        ORDER BY pending.delivery_sequence
                    ) AS delivery_order,
                    row_number() OVER (
                        ORDER BY member.member_position
                    ) AS member_order
                  FROM context_frontier_member AS member
                  JOIN session_pending_delivery AS pending
                    ON pending.recipient_session_id = checked_session
                   AND delegation_delivery_semantic_entry(
                        pending.recipient_session_id,
                        pending.delivery_sequence
                   ) = member.semantic_entry_id
                 WHERE member.owning_session_id = checked_session
                   AND member.context_frontier_id = checked_frontier
                   AND member.member_position > suffix_start
                   AND member.source_session_id = checked_session
          ) AS ordered
         WHERE ordered.delivery_order <> ordered.member_order
    )
$$;


--
-- Name: model_call_suffix_entry_is_valid(uuid, uuid, uuid, uuid, uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION model_call_suffix_entry_is_valid(checked_session uuid, checked_turn uuid, checked_model_call uuid, checked_source_session uuid, checked_entry uuid) RETURNS boolean
    LANGUAGE sql STABLE
    AS $$
    SELECT checked_source_session = checked_session AND (
        EXISTS (
            SELECT 1
              FROM semantic_transcript_entry AS entry
              JOIN accepted_input AS accepted
                ON accepted.accepted_input_id = entry.origin_accepted_input_id
               AND accepted.session_id = entry.source_session_id
             WHERE entry.source_session_id = checked_source_session
               AND entry.semantic_entry_id = checked_entry
               AND entry.payload_kind = 'steering_accepted_input'
               AND entry.steering_source_turn_id = checked_turn
               AND accepted.disposition_kind = 'consumed_as_steering'
               AND accepted.expected_active_turn_id = checked_turn
               AND accepted.consuming_model_call_id = checked_model_call
        ) OR EXISTS (
            SELECT 1
              FROM session_pending_delivery AS pending
             WHERE pending.recipient_session_id = checked_session
               AND delegation_delivery_semantic_entry(
                    pending.recipient_session_id,
                    pending.delivery_sequence
               ) = checked_entry
        )
    )
$$;


--
-- Name: project_terminal_context_compaction_usage(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION project_terminal_context_compaction_usage() RETURNS trigger
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
        NEW.model_call_id, 'context_compaction', NEW.session_id, NULL,
        NEW.resolved_provider_model_identity_id,
        bounded_web_usage_profile(NEW.credential_reference),
        'reported', NEW.usage_input_includes_cache_tokens,
        NEW.input_tokens, NEW.output_tokens,
        NEW.cache_creation_input_tokens, NEW.cache_read_input_tokens
    );
    RETURN NEW;
END;
$$;


--
-- Name: project_terminal_model_call_usage(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION project_terminal_model_call_usage() RETURNS trigger
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
        NEW.model_call_id, 'model_call', NEW.session_id, NEW.turn_id,
        NEW.resolved_provider_model_identity_id,
        bounded_web_usage_profile(NEW.credential_reference),
        NEW.usage_provenance_kind, NEW.usage_input_includes_cache_tokens,
        NEW.usage_input_tokens, NEW.usage_output_tokens,
        NEW.usage_cache_creation_input_tokens,
        NEW.usage_cache_read_input_tokens
    );
    RETURN NEW;
END;
$$;


--
-- Name: reject_context_compaction_input_semantics_change(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_context_compaction_input_semantics_change() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF OLD.usage_input_includes_cache_tokens IS DISTINCT FROM
       NEW.usage_input_includes_cache_tokens
    THEN
        RAISE EXCEPTION 'compaction model call input semantics are immutable'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'context_compaction_input_semantics_immutable';
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: reject_context_compaction_model_call_invalid_change(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_context_compaction_model_call_invalid_change() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        IF NEW.state_kind <> 'prepared'
            OR NEW.terminal_disposition_kind IS NOT NULL
            OR NEW.input_tokens IS NOT NULL
            OR NEW.output_tokens IS NOT NULL
            OR NEW.cache_read_input_tokens IS NOT NULL
            OR NEW.cache_creation_input_tokens IS NOT NULL
        THEN
            RAISE EXCEPTION 'compaction model call must be inserted as Prepared'
                USING ERRCODE = '23514';
        END IF;
        RETURN NEW;
    END IF;

    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'compaction model call is not deletable'
            USING ERRCODE = '23514';
    END IF;

    IF ROW(
        OLD.model_call_id,
        OLD.session_id,
        OLD.direct_model_selection_id,
        OLD.resolved_provider_model_identity_id,
        OLD.source_frontier_id,
        OLD.credential_reference
    ) IS DISTINCT FROM ROW(
        NEW.model_call_id,
        NEW.session_id,
        NEW.direct_model_selection_id,
        NEW.resolved_provider_model_identity_id,
        NEW.source_frontier_id,
        NEW.credential_reference
    ) THEN
        RAISE EXCEPTION 'compaction model call authorization facts are immutable'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.state_kind = 'terminal' THEN
        RAISE EXCEPTION 'terminal compaction model call is immutable'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.state_kind = 'prepared'
       AND NEW.state_kind = 'terminal'
       AND NEW.terminal_disposition_kind NOT IN ('known_failed', 'cancelled')
    THEN
        RAISE EXCEPTION 'prepared compaction call cannot record provider outcome'
            USING ERRCODE = '23514';
    END IF;

    IF NOT (
        (OLD.state_kind = 'prepared' AND NEW.state_kind IN ('in_flight', 'terminal'))
        OR (OLD.state_kind = 'in_flight' AND NEW.state_kind = 'terminal')
    ) THEN
        RAISE EXCEPTION 'invalid compaction model call transition'
            USING ERRCODE = '23514';
    END IF;

    IF NEW.state_kind <> 'terminal' AND (
        NEW.input_tokens IS NOT NULL
        OR NEW.output_tokens IS NOT NULL
        OR NEW.cache_read_input_tokens IS NOT NULL
        OR NEW.cache_creation_input_tokens IS NOT NULL
    ) THEN
        RAISE EXCEPTION 'compaction usage is terminal evidence'
            USING ERRCODE = '23514';
    END IF;

    RETURN NEW;
END;
$$;


--
-- Name: reject_context_frontier_member_out_of_bounds(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_context_frontier_member_out_of_bounds() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    declared_member_count numeric(20, 0);
    prefix_member_count numeric(20, 0) := 0;
BEGIN
    SELECT frontier.member_count,
           COALESCE(prefix.member_count, 0)
      INTO declared_member_count, prefix_member_count
      FROM context_frontier AS frontier
      LEFT JOIN context_frontier AS prefix
        ON prefix.owning_session_id = frontier.owning_session_id
       AND prefix.context_frontier_id =
               frontier.prefix_context_frontier_id
     WHERE frontier.owning_session_id = NEW.owning_session_id
       AND frontier.context_frontier_id = NEW.context_frontier_id;

    -- Deltas may precede their deferred-FK header in one transaction. The
    -- header's deferred completeness check validates that ordering.
    IF FOUND
       AND (
           NEW.member_position <= prefix_member_count
           OR NEW.member_position > declared_member_count
       )
    THEN
        RAISE EXCEPTION
            'context frontier delta position % lies outside (%, %]',
            NEW.member_position,
            prefix_member_count,
            declared_member_count
            USING
                ERRCODE = '23514',
                CONSTRAINT = 'context_frontier_member_within_declared_count';
    END IF;

    RETURN NEW;
END;
$$;


--
-- Name: reject_model_call_instruction_manifest_change(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_model_call_instruction_manifest_change() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NEW.turn_instruction_manifest_id IS DISTINCT FROM OLD.turn_instruction_manifest_id THEN
        RAISE EXCEPTION 'model call instruction manifest is immutable'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: reject_model_call_invalid_change(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_model_call_invalid_change() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        IF NEW.state_kind <> 'prepared' THEN
            RAISE EXCEPTION 'model call must be inserted as Prepared'
                USING
                    ERRCODE = '23514',
                    CONSTRAINT = 'model_call_inserted_prepared';
        END IF;
        RETURN NEW;
    END IF;

    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'model_call is not deletable'
            USING ERRCODE = '23514';
    END IF;

    IF ROW(
        OLD.model_call_id,
        OLD.turn_id,
        OLD.session_id,
        OLD.turn_attempt_id,
        OLD.selection_kind,
        OLD.direct_model_selection_id,
        OLD.frozen_model_alias_id,
        OLD.frozen_alias_selected_direct_id,
        OLD.resolved_provider_model_identity_id,
        OLD.context_frontier_id,
        OLD.credential_reference
    ) IS DISTINCT FROM ROW(
        NEW.model_call_id,
        NEW.turn_id,
        NEW.session_id,
        NEW.turn_attempt_id,
        NEW.selection_kind,
        NEW.direct_model_selection_id,
        NEW.frozen_model_alias_id,
        NEW.frozen_alias_selected_direct_id,
        NEW.resolved_provider_model_identity_id,
        NEW.context_frontier_id,
        NEW.credential_reference
    ) THEN
        RAISE EXCEPTION 'model call authorization facts are immutable'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.state_kind = 'terminal' THEN
        RAISE EXCEPTION 'terminal model call is immutable'
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
            AND NEW.state_kind IN ('cancellation_requested', 'terminal')
        )
        OR (
            OLD.state_kind = 'cancellation_requested'
            AND NEW.state_kind = 'terminal'
        )
    ) THEN
        RAISE EXCEPTION 'model call transition is not monotonic'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.state_kind = 'prepared'
       AND NEW.state_kind = 'terminal'
       AND NEW.terminal_disposition_kind NOT IN ('known_failed', 'cancelled')
    THEN
        RAISE EXCEPTION 'an unsent call has an impossible terminal disposition'
            USING ERRCODE = '23514';
    END IF;

    RETURN NEW;
END;
$$;


--
-- Name: reject_model_call_unsent_provider_failure_cause(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_model_call_unsent_provider_failure_cause() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF OLD.state_kind = 'prepared'
       AND NEW.state_kind = 'terminal'
       AND NEW.terminal_provider_failure_cause IS NOT NULL
    THEN
        RAISE EXCEPTION 'an unsent call cannot carry a provider-failure cause'
            USING
                ERRCODE = '23514',
                CONSTRAINT = 'model_call_unsent_provider_failure_cause_absent';
    END IF;

    RETURN NEW;
END;
$$;


--
-- Name: reject_model_call_unsent_usage(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_model_call_unsent_usage() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF OLD.state_kind = 'prepared'
       AND NEW.state_kind = 'terminal'
       AND (
           NEW.usage_input_tokens IS NOT NULL
           OR NEW.usage_output_tokens IS NOT NULL
           OR NEW.usage_cache_creation_input_tokens IS NOT NULL
           OR NEW.usage_cache_read_input_tokens IS NOT NULL
       )
    THEN
        RAISE EXCEPTION 'an unsent call cannot carry provider-reported token usage'
            USING
                ERRCODE = '23514',
                CONSTRAINT = 'model_call_unsent_usage_unreported';
    END IF;

    RETURN NEW;
END;
$$;


--
-- Name: reject_model_call_usage_metadata_rewrite(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_model_call_usage_metadata_rewrite() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NEW.usage_input_includes_cache_tokens
           IS DISTINCT FROM OLD.usage_input_includes_cache_tokens
       OR (
           NEW.usage_provenance_kind IS DISTINCT FROM OLD.usage_provenance_kind
           AND NOT (
               OLD.state_kind <> 'terminal'
               AND NEW.state_kind = 'terminal'
           )
       ) THEN
        RAISE EXCEPTION 'model-call usage metadata is immutable'
            USING
                ERRCODE = '23514',
                CONSTRAINT = 'model_call_usage_metadata_immutable';
    END IF;
    RETURN NEW;
END;
$$;


--
-- Name: require_context_compaction_exact_evidence(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_context_compaction_exact_evidence() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    source_count numeric(20, 0);
    result_count numeric(20, 0);
    through_position numeric(20, 0);
    predecessor_result uuid;
    predecessor_summary_entry uuid;
    consumed_through_source_session uuid;
    consumed_through_entry uuid;
    predecessor_consumed_position numeric(20, 0);
    mismatch_count bigint;
    visible_entries text[];
    visible_first integer;
    visible_through integer;
    visible_summary integer;
    summary_record record;
    summary_reference text;
    visible_suffix text[];
BEGIN
    SELECT count(*)
      INTO mismatch_count
      FROM context_compaction_model_call AS call
     WHERE call.model_call_id = NEW.producing_call_id
       AND call.session_id = NEW.session_id
       AND call.source_frontier_id = NEW.source_frontier_id
       AND call.state_kind = 'terminal'
       AND call.terminal_disposition_kind = 'completed';
    IF mismatch_count <> 1 THEN
        RAISE EXCEPTION 'compaction requires its exact completed dedicated call'
            USING ERRCODE = '23514';
    END IF;

    SELECT count(*)
      INTO mismatch_count
      FROM semantic_transcript_entry AS summary
     WHERE summary.source_session_id = NEW.session_id
       AND summary.semantic_entry_id = NEW.summary_entry_id
       AND summary.payload_kind = 'context_summary'
       AND summary.context_summary_producing_call_id = NEW.producing_call_id
       AND summary.context_summary_first_source_session_id = NEW.first_source_session_id
       AND summary.context_summary_first_entry_id = NEW.first_entry_id
       AND summary.context_summary_through_source_session_id = NEW.through_source_session_id
       AND summary.context_summary_through_entry_id = NEW.through_entry_id;
    IF mismatch_count <> 1 THEN
        RAISE EXCEPTION 'compaction requires its exact summary provenance'
            USING ERRCODE = '23514';
    END IF;

    SELECT member_count INTO source_count
      FROM context_frontier
     WHERE owning_session_id = NEW.session_id
       AND context_frontier_id = NEW.source_frontier_id;
    SELECT member_count INTO result_count
      FROM context_frontier
     WHERE owning_session_id = NEW.session_id
       AND context_frontier_id = NEW.result_frontier_id;
    through_position := context_frontier_member_position(
        NEW.session_id,
        NEW.source_frontier_id,
        NEW.through_source_session_id,
        NEW.through_entry_id
    );
    IF source_count IS NULL
       OR result_count IS NULL
       OR through_position IS NULL
       OR result_count <> source_count + 1
    THEN
        RAISE EXCEPTION 'compaction range or frontier cardinality is invalid'
            USING ERRCODE = '23514';
    END IF;

    IF NEW.predecessor_compaction_id IS NULL THEN
        SELECT array_agg(
                   member.source_session_id::text || '/' ||
                   member.semantic_entry_id::text
                   ORDER BY member.member_position
               )
          INTO visible_entries
          FROM context_frontier_member AS member
         WHERE member.owning_session_id = NEW.session_id
           AND member.context_frontier_id = NEW.source_frontier_id;

        FOR summary_record IN
            SELECT
                member.source_session_id,
                member.semantic_entry_id,
                entry.context_summary_first_source_session_id AS first_session,
                entry.context_summary_first_entry_id AS first_entry,
                entry.context_summary_through_source_session_id AS through_session,
                entry.context_summary_through_entry_id AS through_entry
              FROM context_frontier_member AS member
              JOIN semantic_transcript_entry AS entry
                ON entry.source_session_id = member.source_session_id
               AND entry.semantic_entry_id = member.semantic_entry_id
             WHERE member.owning_session_id = NEW.session_id
               AND member.context_frontier_id = NEW.source_frontier_id
               AND entry.payload_kind = 'context_summary'
             ORDER BY member.member_position
        LOOP
            summary_reference := summary_record.source_session_id::text || '/' ||
                summary_record.semantic_entry_id::text;
            visible_first := array_position(
                visible_entries,
                summary_record.first_session::text || '/' || summary_record.first_entry::text
            );
            visible_through := array_position(
                visible_entries,
                summary_record.through_session::text || '/' || summary_record.through_entry::text
            );
            visible_summary := array_position(visible_entries, summary_reference);
            IF visible_first IS NULL
               OR visible_first <> 1
               OR visible_through IS NULL
               OR visible_summary IS NULL
               OR visible_summary <= visible_through
            THEN
                RAISE EXCEPTION 'stored compaction summary has an invalid visible range'
                    USING ERRCODE = '23514';
            END IF;

            SELECT array_agg(visible.reference ORDER BY visible.ordinal)
              INTO visible_suffix
              FROM unnest(
                       visible_entries[
                           visible_through + 1:array_length(visible_entries, 1)
                       ]
                   ) WITH ORDINALITY AS visible(reference, ordinal)
             WHERE visible.reference <> summary_reference;
            visible_entries := ARRAY[summary_reference] ||
                COALESCE(visible_suffix, ARRAY[]::text[]);
        END LOOP;

        visible_first := array_position(
            visible_entries,
            NEW.first_source_session_id::text || '/' || NEW.first_entry_id::text
        );
        visible_through := array_position(
            visible_entries,
            NEW.through_source_session_id::text || '/' || NEW.through_entry_id::text
        );
        IF visible_first IS NULL OR visible_first <> 1 OR visible_through IS NULL THEN
            RAISE EXCEPTION 'compaction range must start at the visible frontier start'
                USING ERRCODE = '23514';
        END IF;

        SELECT
            count(*) FILTER (WHERE entry.payload_kind = 'assistant_tool_use')
            - count(*) FILTER (
                WHERE entry.payload_kind IN (
                    'tool_execution_result', 'tool_denied', 'tool_closed_by_turn_end'
                )
                   OR (
                        entry.payload_kind = 'delegation_result'
                        AND entry.tool_result_request_id IS NOT NULL
                   )
            )
          INTO mismatch_count
          FROM unnest(visible_entries[visible_first:visible_through])
                   WITH ORDINALITY AS visible(reference, ordinal)
          JOIN semantic_transcript_entry AS entry
            ON entry.source_session_id = split_part(visible.reference, '/', 1)::uuid
           AND entry.semantic_entry_id = split_part(visible.reference, '/', 2)::uuid;
    ELSE
        SELECT
            predecessor.result_frontier_id,
            predecessor.summary_entry_id
          INTO
            predecessor_result,
            predecessor_summary_entry
          FROM context_compaction AS predecessor
         WHERE predecessor.context_compaction_id = NEW.predecessor_compaction_id
           AND predecessor.session_id = NEW.session_id;
        WITH RECURSIVE predecessor_chain AS MATERIALIZED (
            SELECT
                predecessor.context_compaction_id,
                predecessor.predecessor_compaction_id,
                predecessor.through_source_session_id,
                predecessor.through_entry_id,
                0 AS depth
              FROM context_compaction AS predecessor
             WHERE predecessor.context_compaction_id =
                       NEW.predecessor_compaction_id
               AND predecessor.session_id = NEW.session_id
            UNION ALL
            SELECT
                ancestor.context_compaction_id,
                ancestor.predecessor_compaction_id,
                ancestor.through_source_session_id,
                ancestor.through_entry_id,
                chain.depth + 1
              FROM predecessor_chain AS chain
              JOIN context_compaction AS ancestor
                ON ancestor.context_compaction_id =
                       chain.predecessor_compaction_id
               AND ancestor.session_id = NEW.session_id
        )
        SELECT
            chain.through_source_session_id,
            chain.through_entry_id
          INTO consumed_through_source_session, consumed_through_entry
          FROM predecessor_chain AS chain
         WHERE NOT EXISTS (
                SELECT 1
                  FROM context_compaction AS consumed_summary
                 WHERE consumed_summary.session_id = NEW.session_id
                   AND consumed_summary.summary_entry_id =
                           chain.through_entry_id
                   AND chain.through_source_session_id = NEW.session_id
         )
         ORDER BY chain.depth
         LIMIT 1;
        predecessor_consumed_position := context_frontier_member_position(
            NEW.session_id,
            NEW.source_frontier_id,
            consumed_through_source_session,
            consumed_through_entry
        );
        IF predecessor_result IS NULL
           OR predecessor_consumed_position IS NULL
           OR NOT context_frontier_preserves_prefix(
                NEW.session_id,
                predecessor_result,
                NEW.source_frontier_id
           )
           OR NEW.first_source_session_id <> NEW.session_id
           OR NEW.first_entry_id <> predecessor_summary_entry
        THEN
            RAISE EXCEPTION 'compaction predecessor result and visible start must match'
                USING ERRCODE = '23514';
        END IF;
        IF ROW(NEW.through_source_session_id, NEW.through_entry_id) <>
               ROW(NEW.session_id, predecessor_summary_entry)
           AND (
                through_position <= predecessor_consumed_position
                OR EXISTS (
                    SELECT 1
                      FROM context_compaction AS consumed_summary
                     WHERE consumed_summary.session_id = NEW.session_id
                       AND consumed_summary.summary_entry_id = NEW.through_entry_id
                       AND NEW.through_source_session_id = NEW.session_id
                )
           )
        THEN
            RAISE EXCEPTION 'compaction range must start at the visible frontier start'
                USING ERRCODE = '23514';
        END IF;

        WITH RECURSIVE visible_chain AS MATERIALIZED (
            SELECT
                frontier.context_frontier_id,
                frontier.prefix_context_frontier_id,
                frontier.member_count
              FROM context_frontier AS frontier
             WHERE frontier.owning_session_id = NEW.session_id
               AND frontier.context_frontier_id = NEW.source_frontier_id
            UNION ALL
            SELECT
                prefix.context_frontier_id,
                prefix.prefix_context_frontier_id,
                prefix.member_count
              FROM visible_chain AS chain
              JOIN context_frontier AS prefix
                ON prefix.owning_session_id = NEW.session_id
               AND prefix.context_frontier_id = chain.prefix_context_frontier_id
             WHERE chain.member_count > predecessor_consumed_position
        ),
        current_boundary_entries AS MATERIALIZED (
            SELECT member.source_session_id, member.semantic_entry_id
              FROM visible_chain AS chain
             JOIN context_frontier_delta AS member
                ON member.owning_session_id = NEW.session_id
               AND member.context_frontier_id = chain.context_frontier_id
             WHERE ROW(NEW.through_source_session_id, NEW.through_entry_id) <>
                       ROW(NEW.session_id, predecessor_summary_entry)
               AND member.member_position > predecessor_consumed_position
               AND member.member_position <= through_position
               AND NOT EXISTS (
                    SELECT 1
                      FROM context_compaction AS consumed_summary
                     WHERE consumed_summary.session_id = NEW.session_id
                       AND consumed_summary.summary_entry_id = member.semantic_entry_id
                       AND member.source_session_id = NEW.session_id
               )
            UNION ALL
            SELECT NEW.session_id, predecessor_summary_entry
        )
        SELECT
            count(*) FILTER (WHERE entry.payload_kind = 'assistant_tool_use')
            - count(*) FILTER (
                WHERE entry.payload_kind IN (
                    'tool_execution_result', 'tool_denied', 'tool_closed_by_turn_end'
                )
                   OR (
                        entry.payload_kind = 'delegation_result'
                        AND entry.tool_result_request_id IS NOT NULL
                   )
            )
          INTO mismatch_count
          FROM current_boundary_entries AS visible
          JOIN semantic_transcript_entry AS entry
            ON entry.source_session_id = visible.source_session_id
           AND entry.semantic_entry_id = visible.semantic_entry_id;
    END IF;

    IF mismatch_count <> 0 THEN
        RAISE EXCEPTION 'compaction boundary leaves a tool exchange open'
            USING ERRCODE = '23514';
    END IF;

    IF NOT context_frontier_preserves_prefix(
            NEW.session_id,
            NEW.source_frontier_id,
            NEW.result_frontier_id
       )
       OR context_frontier_member_position(
            NEW.session_id,
            NEW.result_frontier_id,
            NEW.session_id,
            NEW.summary_entry_id
       ) <> result_count
    THEN
        RAISE EXCEPTION 'compaction result must be the source plus its summary'
            USING ERRCODE = '23514';
    END IF;

    RETURN NULL;
END;
$$;


--
-- Name: require_context_frontier_complete_membership(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_context_frontier_complete_membership() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    PERFORM assert_context_frontier_complete_membership(
        CASE WHEN TG_OP = 'DELETE' THEN OLD.owning_session_id ELSE NEW.owning_session_id END,
        CASE WHEN TG_OP = 'DELETE' THEN OLD.context_frontier_id ELSE NEW.context_frontier_id END
    );

    RETURN NULL;
END;
$$;


--
-- Name: require_context_frontier_member_within_declared_count(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_context_frontier_member_within_declared_count() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    declared_member_count numeric(20, 0);
    prefix_member_count numeric(20, 0) := 0;
BEGIN
    SELECT frontier.member_count,
           COALESCE(prefix.member_count, 0)
      INTO declared_member_count, prefix_member_count
      FROM context_frontier AS frontier
      LEFT JOIN context_frontier AS prefix
        ON prefix.owning_session_id = frontier.owning_session_id
       AND prefix.context_frontier_id =
               frontier.prefix_context_frontier_id
     WHERE frontier.owning_session_id = NEW.owning_session_id
       AND frontier.context_frontier_id = NEW.context_frontier_id;

    IF NOT FOUND THEN
        RAISE EXCEPTION
            'context frontier header is unavailable for deferred delta validation'
            USING
                ERRCODE = '23503',
                CONSTRAINT = 'context_frontier_member_requires_visible_header';
    END IF;

    IF NEW.member_position <= prefix_member_count
       OR NEW.member_position > declared_member_count
    THEN
        RAISE EXCEPTION
            'context frontier delta position % lies outside (%, %]',
            NEW.member_position,
            prefix_member_count,
            declared_member_count
            USING
                ERRCODE = '23514',
                CONSTRAINT = 'context_frontier_member_within_declared_count';
    END IF;

    RETURN NULL;
END;
$$;


--
-- Name: require_context_summary_exact_compaction(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_context_summary_exact_compaction() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    matching_compactions bigint;
BEGIN
    IF NEW.payload_kind <> 'context_summary' THEN
        RETURN NULL;
    END IF;

    SELECT count(*)
      INTO matching_compactions
      FROM context_compaction AS compaction
     WHERE compaction.session_id = NEW.source_session_id
       AND compaction.summary_entry_id = NEW.semantic_entry_id
       AND compaction.producing_call_id =
           NEW.context_summary_producing_call_id
       AND compaction.first_source_session_id =
           NEW.context_summary_first_source_session_id
       AND compaction.first_entry_id = NEW.context_summary_first_entry_id
       AND compaction.through_source_session_id =
           NEW.context_summary_through_source_session_id
       AND compaction.through_entry_id =
           NEW.context_summary_through_entry_id;
    IF matching_compactions <> 1 THEN
        RAISE EXCEPTION 'context summary requires its exact compaction record'
            USING ERRCODE = '23514';
    END IF;

    RETURN NULL;
END;
$$;


--
-- Name: require_contiguous_assistant_response_text_positions(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_contiguous_assistant_response_text_positions() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    previous_end numeric;
    next_start numeric;
BEGIN
    SELECT entry.assistant_response_text_start_bytes
               + octet_length(entry.assistant_text_value)::numeric
      INTO previous_end
      FROM semantic_transcript_entry AS entry
     WHERE entry.producing_model_call_id = NEW.producing_model_call_id
       AND entry.payload_kind = 'assistant_text'
       AND entry.assistant_response_part_ordinal
           < NEW.assistant_response_part_ordinal
     ORDER BY entry.assistant_response_part_ordinal DESC
     LIMIT 1;

    IF coalesce(previous_end, 0)
        <> NEW.assistant_response_text_start_bytes THEN
        RAISE EXCEPTION
            'assistant response text positions are not contiguous for model call %',
            NEW.producing_model_call_id
            USING
                ERRCODE = '23514',
                CONSTRAINT =
                    'semantic_transcript_response_text_positions_contiguous';
    END IF;

    SELECT entry.assistant_response_text_start_bytes
      INTO next_start
      FROM semantic_transcript_entry AS entry
     WHERE entry.producing_model_call_id = NEW.producing_model_call_id
       AND entry.payload_kind = 'assistant_text'
       AND entry.assistant_response_part_ordinal
           > NEW.assistant_response_part_ordinal
     ORDER BY entry.assistant_response_part_ordinal ASC
     LIMIT 1;

    IF next_start IS NOT NULL
       AND NEW.assistant_response_text_start_bytes
               + octet_length(NEW.assistant_text_value)::numeric
           <> next_start THEN
        RAISE EXCEPTION
            'assistant response text positions are not contiguous for model call %',
            NEW.producing_model_call_id
            USING
                ERRCODE = '23514',
                CONSTRAINT =
                    'semantic_transcript_response_text_positions_contiguous';
    END IF;

    RETURN NULL;
END;
$$;


--
-- Name: require_model_call_final_state(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_model_call_final_state() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    PERFORM assert_model_call_final_state(
        CASE WHEN TG_OP = 'DELETE' THEN OLD.model_call_id ELSE NEW.model_call_id END
    );
    PERFORM assert_turn_lifecycle_final_state(
        CASE WHEN TG_OP = 'DELETE' THEN OLD.turn_id ELSE NEW.turn_id END
    );
    RETURN NULL;
END;
$$;


--
-- Name: reserve_model_call_identity(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reserve_model_call_identity() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    INSERT INTO model_call_identity (model_call_id, call_kind)
    VALUES (NEW.model_call_id, TG_ARGV[0]);
    RETURN NEW;
END;
$$;


--
-- Name: resolve_context_frontier_members(uuid, uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION resolve_context_frontier_members(requested_owning_session_id uuid, requested_context_frontier_id uuid) RETURNS TABLE(owning_session_id uuid, context_frontier_id uuid, member_position numeric, source_session_id uuid, semantic_entry_id uuid)
    LANGUAGE sql STABLE
    AS $$
    WITH RECURSIVE ancestry (
        owning_session_id,
        context_frontier_id,
        prefix_context_frontier_id
    ) AS (
        SELECT
            frontier.owning_session_id,
            frontier.context_frontier_id,
            frontier.prefix_context_frontier_id
          FROM context_frontier AS frontier
         WHERE frontier.owning_session_id =
                   requested_owning_session_id
           AND frontier.context_frontier_id =
                   requested_context_frontier_id
        UNION
        SELECT
            prefix.owning_session_id,
            prefix.context_frontier_id,
            prefix.prefix_context_frontier_id
          FROM ancestry
          JOIN context_frontier AS prefix
            ON prefix.owning_session_id = ancestry.owning_session_id
           AND prefix.context_frontier_id =
                   ancestry.prefix_context_frontier_id
    )
    SELECT
        requested_owning_session_id,
        requested_context_frontier_id,
        delta.member_position,
        delta.source_session_id,
        delta.semantic_entry_id
      FROM ancestry
      JOIN context_frontier_delta AS delta
        ON delta.owning_session_id = ancestry.owning_session_id
       AND delta.context_frontier_id = ancestry.context_frontier_id
     ORDER BY delta.member_position
$$;


--
-- Tables.
--

--
-- Name: context_compacted_outbox_event; Type: TABLE; Schema: public
--

CREATE TABLE context_compacted_outbox_event (
    event_sequence numeric(20,0) NOT NULL,
    event_kind text NOT NULL,
    storage_version smallint NOT NULL,
    session_id uuid NOT NULL,
    context_compaction_id uuid NOT NULL,
    model_call_id uuid NOT NULL,
    through_position numeric(20,0) NOT NULL,
    summary_entry_id uuid NOT NULL,
    result_frontier_id uuid NOT NULL,
    CONSTRAINT context_compacted_outbox_kind_closed CHECK ((event_kind = 'context_compacted'::text)),
    CONSTRAINT context_compacted_outbox_position_u64 CHECK (((through_position >= (1)::numeric) AND (through_position <= '18446744073709551615'::numeric))),
    CONSTRAINT context_compacted_outbox_version_supported CHECK ((storage_version = 1))
);


--
-- Name: context_compaction; Type: TABLE; Schema: public
--

CREATE TABLE context_compaction (
    context_compaction_id uuid NOT NULL,
    session_id uuid NOT NULL,
    predecessor_compaction_id uuid,
    source_frontier_id uuid NOT NULL,
    result_frontier_id uuid NOT NULL,
    producing_call_id uuid NOT NULL,
    first_source_session_id uuid NOT NULL,
    first_entry_id uuid NOT NULL,
    through_source_session_id uuid NOT NULL,
    through_entry_id uuid NOT NULL,
    summary_entry_id uuid NOT NULL,
    CONSTRAINT context_compaction_not_same_frontier CHECK ((source_frontier_id <> result_frontier_id))
);


--
-- Name: context_compaction_model_call; Type: TABLE; Schema: public
--

CREATE TABLE context_compaction_model_call (
    model_call_id uuid NOT NULL,
    session_id uuid NOT NULL,
    direct_model_selection_id uuid CONSTRAINT context_compaction_model_cal_direct_model_selection_id_not_null NOT NULL,
    resolved_provider_model_identity_id uuid CONSTRAINT context_compaction_model_ca_resolved_provider_model_id_not_null NOT NULL,
    source_frontier_id uuid NOT NULL,
    credential_reference text NOT NULL,
    state_kind text NOT NULL,
    terminal_disposition_kind text,
    input_tokens numeric(20,0),
    output_tokens numeric(20,0),
    cache_read_input_tokens numeric(20,0),
    cache_creation_input_tokens numeric(20,0),
    usage_input_includes_cache_tokens boolean DEFAULT false,
    CONSTRAINT context_compaction_model_call_credential_reference_nonempty CHECK ((char_length(credential_reference) > 0)),
    CONSTRAINT context_compaction_model_call_disposition_closed CHECK (((terminal_disposition_kind IS NULL) OR (terminal_disposition_kind = ANY (ARRAY['completed'::text, 'known_failed'::text, 'refused'::text, 'cancelled'::text, 'ambiguous'::text])))),
    CONSTRAINT context_compaction_model_call_state_closed CHECK ((state_kind = ANY (ARRAY['prepared'::text, 'in_flight'::text, 'terminal'::text]))),
    CONSTRAINT context_compaction_model_call_state_shape CHECK ((((state_kind <> 'terminal'::text) AND (terminal_disposition_kind IS NULL)) OR ((state_kind = 'terminal'::text) AND (terminal_disposition_kind IS NOT NULL)))),
    CONSTRAINT context_compaction_model_call_usage_u64 CHECK ((((input_tokens IS NULL) OR ((input_tokens = trunc(input_tokens)) AND ((input_tokens >= (0)::numeric) AND (input_tokens <= '18446744073709551615'::numeric)))) AND ((output_tokens IS NULL) OR ((output_tokens = trunc(output_tokens)) AND ((output_tokens >= (0)::numeric) AND (output_tokens <= '18446744073709551615'::numeric)))) AND ((cache_read_input_tokens IS NULL) OR ((cache_read_input_tokens = trunc(cache_read_input_tokens)) AND ((cache_read_input_tokens >= (0)::numeric) AND (cache_read_input_tokens <= '18446744073709551615'::numeric)))) AND ((cache_creation_input_tokens IS NULL) OR ((cache_creation_input_tokens = trunc(cache_creation_input_tokens)) AND ((cache_creation_input_tokens >= (0)::numeric) AND (cache_creation_input_tokens <= '18446744073709551615'::numeric))))))
);


--
-- Name: context_frontier; Type: TABLE; Schema: public
--

CREATE TABLE context_frontier (
    owning_session_id uuid NOT NULL,
    context_frontier_id uuid NOT NULL,
    member_count numeric(20,0) NOT NULL,
    prefix_context_frontier_id uuid,
    CONSTRAINT context_frontier_member_count_u64 CHECK (((member_count >= (0)::numeric) AND (member_count <= '18446744073709551615'::numeric)))
);


--
-- Name: context_frontier_delta; Type: TABLE; Schema: public
--

CREATE TABLE context_frontier_delta (
    owning_session_id uuid CONSTRAINT context_frontier_member_owning_session_id_not_null NOT NULL,
    context_frontier_id uuid CONSTRAINT context_frontier_member_context_frontier_id_not_null NOT NULL,
    member_position numeric(20,0) CONSTRAINT context_frontier_member_member_position_not_null NOT NULL,
    source_session_id uuid CONSTRAINT context_frontier_member_source_session_id_not_null NOT NULL,
    semantic_entry_id uuid CONSTRAINT context_frontier_member_semantic_entry_id_not_null NOT NULL,
    CONSTRAINT context_frontier_member_position_positive_u64 CHECK (((member_position >= (1)::numeric) AND (member_position <= '18446744073709551615'::numeric)))
);


--
-- Name: context_frontier_member; Type: VIEW; Schema: public
--

CREATE VIEW context_frontier_member AS
 SELECT frontier.owning_session_id,
    frontier.context_frontier_id,
    member.member_position,
    member.source_session_id,
    member.semantic_entry_id
   FROM (context_frontier frontier
     CROSS JOIN LATERAL resolve_context_frontier_members(frontier.owning_session_id, frontier.context_frontier_id) member(owning_session_id, context_frontier_id, member_position, source_session_id, semantic_entry_id));


--
-- Name: credential_pool_availability_successor; Type: TABLE; Schema: public
--

CREATE TABLE credential_pool_availability_successor (
    predecessor_model_call_id uuid CONSTRAINT credential_pool_availability_predecessor_model_call_id_not_null NOT NULL,
    successor_turn_attempt_id uuid CONSTRAINT credential_pool_availability_successor_turn_attempt_id_not_null NOT NULL,
    cause_kind text NOT NULL,
    retry_backoff_milliseconds bigint CONSTRAINT credential_pool_availabilit_retry_backoff_milliseconds_not_null NOT NULL,
    retry_not_before timestamp with time zone CONSTRAINT credential_pool_availability_successo_retry_not_before_not_null NOT NULL,
    CONSTRAINT credential_pool_availability_s_retry_backoff_milliseconds_check CHECK ((retry_backoff_milliseconds >= 0)),
    CONSTRAINT credential_pool_availability_successor_cause_kind_check CHECK ((cause_kind = ANY (ARRAY['rate_limited'::text, 'quota_exhausted'::text, 'overloaded'::text, 'provider_internal'::text])))
);


--
-- Name: credential_pool_chain_exclusion; Type: TABLE; Schema: public
--

CREATE TABLE credential_pool_chain_exclusion (
    session_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    credential_reference text NOT NULL,
    predecessor_model_call_id uuid CONSTRAINT credential_pool_chain_exclus_predecessor_model_call_id_not_null NOT NULL,
    cause_kind text NOT NULL,
    CONSTRAINT credential_pool_chain_exclusion_cause_kind_check CHECK ((cause_kind = ANY (ARRAY['rate_limited'::text, 'quota_exhausted'::text, 'overloaded'::text])))
);


--
-- Name: credential_pool_member_action; Type: TABLE; Schema: public
--

CREATE TABLE credential_pool_member_action (
    action_id bigint NOT NULL,
    pool_name text NOT NULL,
    credential_reference text NOT NULL,
    action_kind text NOT NULL,
    observed_session_id uuid NOT NULL,
    observed_turn_id uuid NOT NULL,
    observation_model_call_id uuid CONSTRAINT credential_pool_member_actio_observation_model_call_id_not_null NOT NULL,
    consumed_turn_id uuid,
    cause_kind text NOT NULL,
    CONSTRAINT credential_pool_member_action_action_kind_check CHECK ((action_kind = ANY (ARRAY['switch_next_turn'::text, 'avoid_new_sessions'::text, 'quarantine'::text]))),
    CONSTRAINT credential_pool_member_action_cause_kind_check CHECK ((cause_kind = ANY (ARRAY['rate_limited'::text, 'quota_exhausted'::text, 'overloaded'::text, 'credential_rejected'::text]))),
    CONSTRAINT credential_pool_member_action_check CHECK (((action_kind = 'switch_next_turn'::text) OR (consumed_turn_id IS NULL)))
);


--
-- Name: credential_pool_member_action_action_id_seq; Type: SEQUENCE; Schema: public
--

CREATE SEQUENCE credential_pool_member_action_action_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: credential_pool_member_action_action_id_seq; Type: SEQUENCE OWNED BY; Schema: public
--

ALTER SEQUENCE credential_pool_member_action_action_id_seq OWNED BY credential_pool_member_action.action_id;


--
-- Name: credential_pool_terminal_exhaustion; Type: TABLE; Schema: public
--

CREATE TABLE credential_pool_terminal_exhaustion (
    terminal_attempt_id uuid CONSTRAINT credential_pool_terminal_exhaustio_terminal_attempt_id_not_null NOT NULL,
    terminal_model_call_id uuid,
    session_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    pool_name text NOT NULL,
    cause_kind text,
    CONSTRAINT credential_pool_terminal_exhaustion_cause_kind_check CHECK ((cause_kind = ANY (ARRAY['rate_limited'::text, 'quota_exhausted'::text, 'overloaded'::text])))
);


--
-- Name: model_call; Type: TABLE; Schema: public
--

CREATE TABLE model_call (
    model_call_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    session_id uuid NOT NULL,
    turn_attempt_id uuid NOT NULL,
    selection_kind text NOT NULL,
    direct_model_selection_id uuid,
    frozen_model_alias_id uuid,
    frozen_alias_selected_direct_id uuid,
    resolved_provider_model_identity_id uuid NOT NULL,
    context_frontier_id uuid NOT NULL,
    state_kind text NOT NULL,
    terminal_disposition_kind text,
    credential_reference text NOT NULL,
    usage_input_tokens numeric,
    usage_output_tokens numeric,
    usage_cache_creation_input_tokens numeric,
    usage_cache_read_input_tokens numeric,
    terminal_provider_failure_cause text,
    usage_input_includes_cache_tokens boolean DEFAULT false,
    usage_provenance_kind text DEFAULT 'reported'::text NOT NULL,
    turn_instruction_manifest_id uuid NOT NULL,
    terminal_attachment_preparation_failure_cause text,
    terminal_attachment_preparation_failure_maximum_bytes numeric(20,0),
    CONSTRAINT model_call_attachment_preparation_failure_cause_closed CHECK (((terminal_attachment_preparation_failure_cause IS NULL) OR (terminal_attachment_preparation_failure_cause = ANY (ARRAY['too_large'::text, 'missing'::text, 'corrupt'::text])))),
    CONSTRAINT model_call_attachment_preparation_failure_cause_shape CHECK (((((terminal_attachment_preparation_failure_cause IS NULL) AND (terminal_attachment_preparation_failure_maximum_bytes IS NULL)) OR ((state_kind = 'terminal'::text) AND (terminal_disposition_kind = 'known_failed'::text) AND (terminal_provider_failure_cause IS NULL) AND (((terminal_attachment_preparation_failure_cause = 'too_large'::text) AND (terminal_attachment_preparation_failure_maximum_bytes IS NOT NULL)) OR ((terminal_attachment_preparation_failure_cause = ANY (ARRAY['missing'::text, 'corrupt'::text])) AND (terminal_attachment_preparation_failure_maximum_bytes IS NULL))))) IS TRUE)),
    CONSTRAINT model_call_attachment_preparation_failure_maximum_bytes_u64 CHECK (((terminal_attachment_preparation_failure_maximum_bytes IS NULL) OR ((terminal_attachment_preparation_failure_maximum_bytes >= (1)::numeric) AND (terminal_attachment_preparation_failure_maximum_bytes <= '18446744073709551615'::numeric)))),
    CONSTRAINT model_call_cancelled_usage_is_unreported CHECK (((terminal_disposition_kind IS DISTINCT FROM 'cancelled'::text) OR ((usage_input_tokens IS NULL) AND (usage_output_tokens IS NULL) AND (usage_cache_creation_input_tokens IS NULL) AND (usage_cache_read_input_tokens IS NULL)))),
    CONSTRAINT model_call_credential_reference_nonempty CHECK ((char_length(credential_reference) > 0)),
    CONSTRAINT model_call_provider_failure_cause_closed CHECK (((terminal_provider_failure_cause IS NULL) OR (terminal_provider_failure_cause = ANY (ARRAY['credential_rejected'::text, 'permission_denied'::text, 'invalid_request'::text, 'target_not_found'::text, 'request_too_large'::text, 'rate_limited'::text, 'quota_exhausted'::text, 'overloaded'::text, 'provider_internal'::text, 'unrecognized'::text])))),
    CONSTRAINT model_call_provider_failure_cause_requires_known_failure CHECK (((terminal_provider_failure_cause IS NULL) OR ((state_kind = 'terminal'::text) AND (terminal_disposition_kind = 'known_failed'::text)))),
    CONSTRAINT model_call_selection_kind_closed CHECK ((selection_kind = ANY (ARRAY['direct'::text, 'frozen_alias'::text]))),
    CONSTRAINT model_call_selection_shape CHECK ((((selection_kind = 'direct'::text) AND (direct_model_selection_id IS NOT NULL) AND (frozen_model_alias_id IS NULL) AND (frozen_alias_selected_direct_id IS NULL)) OR ((selection_kind = 'frozen_alias'::text) AND (direct_model_selection_id IS NULL) AND (frozen_model_alias_id IS NOT NULL) AND (frozen_alias_selected_direct_id IS NOT NULL)))),
    CONSTRAINT model_call_state_kind_closed CHECK ((state_kind = ANY (ARRAY['prepared'::text, 'in_flight'::text, 'cancellation_requested'::text, 'terminal'::text]))),
    CONSTRAINT model_call_state_payload_shape CHECK ((((state_kind <> 'terminal'::text) AND (terminal_disposition_kind IS NULL)) OR ((state_kind = 'terminal'::text) AND (terminal_disposition_kind IS NOT NULL)))),
    CONSTRAINT model_call_terminal_disposition_closed CHECK (((terminal_disposition_kind IS NULL) OR (terminal_disposition_kind = ANY (ARRAY['completed'::text, 'known_failed'::text, 'refused'::text, 'cancelled'::text, 'ambiguous'::text])))),
    CONSTRAINT model_call_usage_cache_creation_input_tokens_u64 CHECK (((usage_cache_creation_input_tokens IS NULL) OR ((usage_cache_creation_input_tokens = trunc(usage_cache_creation_input_tokens)) AND ((usage_cache_creation_input_tokens >= (0)::numeric) AND (usage_cache_creation_input_tokens <= '18446744073709551615'::numeric))))),
    CONSTRAINT model_call_usage_cache_read_input_tokens_u64 CHECK (((usage_cache_read_input_tokens IS NULL) OR ((usage_cache_read_input_tokens = trunc(usage_cache_read_input_tokens)) AND ((usage_cache_read_input_tokens >= (0)::numeric) AND (usage_cache_read_input_tokens <= '18446744073709551615'::numeric))))),
    CONSTRAINT model_call_usage_input_tokens_u64 CHECK (((usage_input_tokens IS NULL) OR ((usage_input_tokens = trunc(usage_input_tokens)) AND ((usage_input_tokens >= (0)::numeric) AND (usage_input_tokens <= '18446744073709551615'::numeric))))),
    CONSTRAINT model_call_usage_is_terminal_evidence CHECK (((state_kind = 'terminal'::text) OR ((usage_input_tokens IS NULL) AND (usage_output_tokens IS NULL) AND (usage_cache_creation_input_tokens IS NULL) AND (usage_cache_read_input_tokens IS NULL)))),
    CONSTRAINT model_call_usage_output_tokens_u64 CHECK (((usage_output_tokens IS NULL) OR ((usage_output_tokens = trunc(usage_output_tokens)) AND ((usage_output_tokens >= (0)::numeric) AND (usage_output_tokens <= '18446744073709551615'::numeric))))),
    CONSTRAINT model_call_usage_provenance_kind_closed CHECK ((usage_provenance_kind = ANY (ARRAY['reported'::text, 'estimated'::text])))
);


--
-- Name: model_call_credential_pool_member; Type: TABLE; Schema: public
--

CREATE TABLE model_call_credential_pool_member (
    model_call_id uuid NOT NULL,
    member_ordinal integer NOT NULL,
    credential_reference text NOT NULL,
    priority bigint NOT NULL,
    CONSTRAINT model_call_credential_pool_member_member_ordinal_check CHECK ((member_ordinal >= 0)),
    CONSTRAINT model_call_credential_pool_member_priority_check CHECK ((priority > 0))
);


--
-- Name: model_call_credential_pool_policy; Type: TABLE; Schema: public
--

CREATE TABLE model_call_credential_pool_policy (
    model_call_id uuid NOT NULL,
    pool_name text NOT NULL,
    on_pool_exhausted text NOT NULL,
    on_quota_exhausted text NOT NULL,
    on_rate_limited text NOT NULL,
    on_overloaded text NOT NULL,
    on_credential_rejected text CONSTRAINT model_call_credential_pool_poli_on_credential_rejected_not_null NOT NULL,
    CONSTRAINT model_call_credential_pool_policy_on_credential_rejected_check CHECK ((on_credential_rejected = ANY (ARRAY['stay'::text, 'switch_next_turn'::text, 'switch_now'::text, 'avoid_new_sessions'::text, 'quarantine'::text]))),
    CONSTRAINT model_call_credential_pool_policy_on_overloaded_check CHECK ((on_overloaded = ANY (ARRAY['stay'::text, 'switch_next_turn'::text, 'switch_now'::text, 'avoid_new_sessions'::text, 'quarantine'::text]))),
    CONSTRAINT model_call_credential_pool_policy_on_pool_exhausted_check CHECK ((on_pool_exhausted = ANY (ARRAY['park'::text, 'fail'::text]))),
    CONSTRAINT model_call_credential_pool_policy_on_quota_exhausted_check CHECK ((on_quota_exhausted = ANY (ARRAY['stay'::text, 'switch_next_turn'::text, 'switch_now'::text, 'avoid_new_sessions'::text, 'quarantine'::text]))),
    CONSTRAINT model_call_credential_pool_policy_on_rate_limited_check CHECK ((on_rate_limited = ANY (ARRAY['stay'::text, 'switch_next_turn'::text, 'switch_now'::text, 'avoid_new_sessions'::text, 'quarantine'::text])))
);


--
-- Name: model_call_identity; Type: TABLE; Schema: public
--

CREATE TABLE model_call_identity (
    model_call_id uuid NOT NULL,
    call_kind text NOT NULL,
    CONSTRAINT model_call_identity_kind_closed CHECK ((call_kind = ANY (ARRAY['ordinary'::text, 'context_compaction'::text, 'approval_judge'::text])))
);


--
-- Name: model_call_transition_outbox_event; Type: TABLE; Schema: public
--

CREATE TABLE model_call_transition_outbox_event (
    event_sequence numeric(20,0) NOT NULL,
    event_kind text NOT NULL,
    storage_version smallint NOT NULL,
    session_id uuid NOT NULL,
    model_call_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    call_state_kind text NOT NULL,
    terminal_disposition_kind text,
    CONSTRAINT model_call_transition_outbox_kind_closed CHECK ((event_kind = 'model_call_transition'::text)),
    CONSTRAINT model_call_transition_outbox_state_closed CHECK ((call_state_kind = ANY (ARRAY['prepared'::text, 'in_flight'::text, 'cancellation_requested'::text, 'terminal'::text]))),
    CONSTRAINT model_call_transition_outbox_state_shape CHECK ((((call_state_kind <> 'terminal'::text) AND (terminal_disposition_kind IS NULL)) OR ((call_state_kind = 'terminal'::text) AND (terminal_disposition_kind = ANY (ARRAY['completed'::text, 'known_failed'::text, 'refused'::text, 'cancelled'::text, 'ambiguous'::text]))))),
    CONSTRAINT model_call_transition_outbox_version_supported CHECK ((storage_version = 1))
);


--
-- Name: model_call_user_override; Type: TABLE; Schema: public
--

CREATE TABLE model_call_user_override (
    model_call_id uuid NOT NULL,
    denied_request_id uuid NOT NULL
);


--
-- Name: credential_pool_member_action action_id; Type: DEFAULT; Schema: public
--

ALTER TABLE ONLY credential_pool_member_action ALTER COLUMN action_id SET DEFAULT nextval('credential_pool_member_action_action_id_seq'::regclass);


--
-- Constraints.
--

--
-- Name: context_compacted_outbox_event context_compacted_outbox_event_context_compaction_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY context_compacted_outbox_event
    ADD CONSTRAINT context_compacted_outbox_event_context_compaction_id_key UNIQUE (context_compaction_id);


--
-- Name: context_compacted_outbox_event context_compacted_outbox_event_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY context_compacted_outbox_event
    ADD CONSTRAINT context_compacted_outbox_event_pkey PRIMARY KEY (event_sequence);


--
-- Name: context_compaction context_compaction_call_once; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY context_compaction
    ADD CONSTRAINT context_compaction_call_once UNIQUE (producing_call_id);


--
-- Name: context_compaction_model_call context_compaction_model_call_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY context_compaction_model_call
    ADD CONSTRAINT context_compaction_model_call_pkey PRIMARY KEY (model_call_id);


--
-- Name: context_compaction_model_call context_compaction_model_call_session_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY context_compaction_model_call
    ADD CONSTRAINT context_compaction_model_call_session_key UNIQUE (model_call_id, session_id);


--
-- Name: context_compaction context_compaction_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY context_compaction
    ADD CONSTRAINT context_compaction_pkey PRIMARY KEY (context_compaction_id);


--
-- Name: context_compaction context_compaction_result_once; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY context_compaction
    ADD CONSTRAINT context_compaction_result_once UNIQUE (result_frontier_id);


--
-- Name: context_compaction context_compaction_session_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY context_compaction
    ADD CONSTRAINT context_compaction_session_key UNIQUE (context_compaction_id, session_id);


--
-- Name: context_compaction context_compaction_summary_once; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY context_compaction
    ADD CONSTRAINT context_compaction_summary_once UNIQUE (summary_entry_id);


--
-- Name: context_frontier context_frontier_id_global; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY context_frontier
    ADD CONSTRAINT context_frontier_id_global UNIQUE (context_frontier_id);


--
-- Name: context_frontier_delta context_frontier_member_entry_once; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY context_frontier_delta
    ADD CONSTRAINT context_frontier_member_entry_once UNIQUE (owning_session_id, context_frontier_id, source_session_id, semantic_entry_id);


--
-- Name: context_frontier_delta context_frontier_member_pk; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY context_frontier_delta
    ADD CONSTRAINT context_frontier_member_pk PRIMARY KEY (owning_session_id, context_frontier_id, member_position);


--
-- Name: context_frontier context_frontier_pk; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY context_frontier
    ADD CONSTRAINT context_frontier_pk PRIMARY KEY (owning_session_id, context_frontier_id);


--
-- Name: credential_pool_availability_successor credential_pool_availability_succ_successor_turn_attempt_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY credential_pool_availability_successor
    ADD CONSTRAINT credential_pool_availability_succ_successor_turn_attempt_id_key UNIQUE (successor_turn_attempt_id);


--
-- Name: credential_pool_availability_successor credential_pool_availability_successor_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY credential_pool_availability_successor
    ADD CONSTRAINT credential_pool_availability_successor_pkey PRIMARY KEY (predecessor_model_call_id);


--
-- Name: credential_pool_chain_exclusion credential_pool_chain_exclusion_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY credential_pool_chain_exclusion
    ADD CONSTRAINT credential_pool_chain_exclusion_pkey PRIMARY KEY (session_id, turn_id, credential_reference);


--
-- Name: credential_pool_chain_exclusion credential_pool_chain_exclusion_predecessor_model_call_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY credential_pool_chain_exclusion
    ADD CONSTRAINT credential_pool_chain_exclusion_predecessor_model_call_id_key UNIQUE (predecessor_model_call_id);


--
-- Name: credential_pool_member_action credential_pool_member_action_observation_model_call_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY credential_pool_member_action
    ADD CONSTRAINT credential_pool_member_action_observation_model_call_id_key UNIQUE (observation_model_call_id);


--
-- Name: credential_pool_member_action credential_pool_member_action_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY credential_pool_member_action
    ADD CONSTRAINT credential_pool_member_action_pkey PRIMARY KEY (action_id);


--
-- Name: credential_pool_terminal_exhaustion credential_pool_terminal_exhaustion_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY credential_pool_terminal_exhaustion
    ADD CONSTRAINT credential_pool_terminal_exhaustion_pkey PRIMARY KEY (terminal_attempt_id);


--
-- Name: credential_pool_terminal_exhaustion credential_pool_terminal_exhaustion_terminal_model_call_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY credential_pool_terminal_exhaustion
    ADD CONSTRAINT credential_pool_terminal_exhaustion_terminal_model_call_id_key UNIQUE (terminal_model_call_id);


--
-- Name: model_call model_call_attempt_once; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY model_call
    ADD CONSTRAINT model_call_attempt_once UNIQUE (turn_attempt_id);


--
-- Name: model_call_credential_pool_member model_call_credential_pool_me_model_call_id_credential_refe_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY model_call_credential_pool_member
    ADD CONSTRAINT model_call_credential_pool_me_model_call_id_credential_refe_key UNIQUE (model_call_id, credential_reference);


--
-- Name: model_call_credential_pool_member model_call_credential_pool_member_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY model_call_credential_pool_member
    ADD CONSTRAINT model_call_credential_pool_member_pkey PRIMARY KEY (model_call_id, member_ordinal);


--
-- Name: model_call_credential_pool_policy model_call_credential_pool_policy_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY model_call_credential_pool_policy
    ADD CONSTRAINT model_call_credential_pool_policy_pkey PRIMARY KEY (model_call_id);


--
-- Name: model_call_identity model_call_identity_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY model_call_identity
    ADD CONSTRAINT model_call_identity_pkey PRIMARY KEY (model_call_id);


--
-- Name: model_call model_call_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY model_call
    ADD CONSTRAINT model_call_pkey PRIMARY KEY (model_call_id);


--
-- Name: model_call model_call_session_correlation_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY model_call
    ADD CONSTRAINT model_call_session_correlation_key UNIQUE (model_call_id, session_id);


--
-- Name: model_call_transition_outbox_event model_call_transition_outbox_event_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY model_call_transition_outbox_event
    ADD CONSTRAINT model_call_transition_outbox_event_pkey PRIMARY KEY (event_sequence);


--
-- Name: model_call_transition_outbox_event model_call_transition_outbox_once; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY model_call_transition_outbox_event
    ADD CONSTRAINT model_call_transition_outbox_once UNIQUE (model_call_id, call_state_kind);


--
-- Name: model_call model_call_turn_correlation_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY model_call
    ADD CONSTRAINT model_call_turn_correlation_key UNIQUE (model_call_id, turn_id, session_id);


--
-- Name: model_call_user_override model_call_user_override_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY model_call_user_override
    ADD CONSTRAINT model_call_user_override_pkey PRIMARY KEY (model_call_id, denied_request_id);


--
-- Indexes.
--

--
-- Name: context_compaction_model_call_live_by_session; Type: INDEX; Schema: public
--

CREATE INDEX context_compaction_model_call_live_by_session ON context_compaction_model_call USING btree (session_id) WHERE (state_kind <> 'terminal'::text);


--
-- Name: context_compaction_one_root_per_session; Type: INDEX; Schema: public
--

CREATE UNIQUE INDEX context_compaction_one_root_per_session ON context_compaction USING btree (session_id) WHERE (predecessor_compaction_id IS NULL);


--
-- Name: context_compaction_one_successor_per_predecessor; Type: INDEX; Schema: public
--

CREATE UNIQUE INDEX context_compaction_one_successor_per_predecessor ON context_compaction USING btree (session_id, predecessor_compaction_id) WHERE (predecessor_compaction_id IS NOT NULL);


--
-- Name: context_compaction_reported_usage_by_session_target; Type: INDEX; Schema: public
--

CREATE INDEX context_compaction_reported_usage_by_session_target ON context_compaction_model_call USING btree (session_id, resolved_provider_model_identity_id, model_call_id DESC) WHERE ((state_kind = 'terminal'::text) AND (input_tokens IS NOT NULL));


--
-- Name: credential_pool_member_action_selection; Type: INDEX; Schema: public
--

CREATE INDEX credential_pool_member_action_selection ON credential_pool_member_action USING btree (pool_name, credential_reference, action_kind) WHERE (consumed_turn_id IS NULL);


--
-- Name: model_call_by_turn_attempt; Type: INDEX; Schema: public
--

CREATE INDEX model_call_by_turn_attempt ON model_call USING btree (turn_id, turn_attempt_id);


--
-- Name: model_call_live_by_session; Type: INDEX; Schema: public
--

CREATE INDEX model_call_live_by_session ON model_call USING btree (session_id) WHERE (state_kind <> 'terminal'::text);


--
-- Name: model_call_usage_by_session_state_turn_call; Type: INDEX; Schema: public
--

CREATE INDEX model_call_usage_by_session_state_turn_call ON model_call USING btree (session_id, state_kind, turn_id, model_call_id);


--
-- Triggers.
--

--
-- Name: context_compacted_outbox_event context_compacted_outbox_event_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER context_compacted_outbox_event_cannot_be_truncated BEFORE TRUNCATE ON context_compacted_outbox_event FOR EACH STATEMENT EXECUTE FUNCTION reject_outbox_table_truncate();


--
-- Name: context_compacted_outbox_event context_compacted_outbox_event_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER context_compacted_outbox_event_is_append_only BEFORE DELETE OR UPDATE ON context_compacted_outbox_event FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: context_compaction_model_call context_compaction_call_reserves_global_identity; Type: TRIGGER; Schema: public
--

CREATE TRIGGER context_compaction_call_reserves_global_identity BEFORE INSERT ON context_compaction_model_call FOR EACH ROW EXECUTE FUNCTION reserve_model_call_identity('context_compaction');


--
-- Name: context_compaction_model_call context_compaction_input_semantics_are_immutable; Type: TRIGGER; Schema: public
--

CREATE TRIGGER context_compaction_input_semantics_are_immutable BEFORE UPDATE ON context_compaction_model_call FOR EACH ROW EXECUTE FUNCTION reject_context_compaction_input_semantics_change();


--
-- Name: context_compaction context_compaction_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER context_compaction_is_append_only BEFORE DELETE OR UPDATE ON context_compaction FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: context_compaction_model_call context_compaction_model_call_changes_are_guarded; Type: TRIGGER; Schema: public
--

CREATE TRIGGER context_compaction_model_call_changes_are_guarded BEFORE INSERT OR DELETE OR UPDATE ON context_compaction_model_call FOR EACH ROW EXECUTE FUNCTION reject_context_compaction_model_call_invalid_change();


--
-- Name: context_compaction_model_call context_compaction_projects_terminal_usage; Type: TRIGGER; Schema: public
--

CREATE TRIGGER context_compaction_projects_terminal_usage AFTER INSERT OR UPDATE ON context_compaction_model_call FOR EACH ROW WHEN ((new.state_kind = 'terminal'::text)) EXECUTE FUNCTION project_terminal_context_compaction_usage();


--
-- Name: context_compaction context_compaction_requires_exact_evidence; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER context_compaction_requires_exact_evidence AFTER INSERT ON context_compaction DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_context_compaction_exact_evidence();


--
-- Name: context_frontier context_frontier_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER context_frontier_is_append_only BEFORE DELETE OR UPDATE ON context_frontier FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: context_frontier_delta context_frontier_member_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER context_frontier_member_is_append_only BEFORE DELETE OR UPDATE ON context_frontier_delta FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: context_frontier_delta context_frontier_member_rechecks_declared_count; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER context_frontier_member_rechecks_declared_count AFTER INSERT ON context_frontier_delta DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_context_frontier_member_within_declared_count();


--
-- Name: context_frontier_delta context_frontier_member_stays_within_declared_count; Type: TRIGGER; Schema: public
--

CREATE TRIGGER context_frontier_member_stays_within_declared_count BEFORE INSERT ON context_frontier_delta FOR EACH ROW EXECUTE FUNCTION reject_context_frontier_member_out_of_bounds();


--
-- Name: context_frontier context_frontier_requires_complete_membership; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER context_frontier_requires_complete_membership AFTER INSERT OR DELETE OR UPDATE ON context_frontier DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_context_frontier_complete_membership();


--
-- Name: semantic_transcript_entry context_summary_requires_exact_compaction; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER context_summary_requires_exact_compaction AFTER INSERT OR UPDATE ON semantic_transcript_entry DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_context_summary_exact_compaction();


--
-- Name: model_call model_call_changes_are_guarded; Type: TRIGGER; Schema: public
--

CREATE TRIGGER model_call_changes_are_guarded BEFORE INSERT OR DELETE OR UPDATE ON model_call FOR EACH ROW EXECUTE FUNCTION reject_model_call_invalid_change();


--
-- Name: model_call_identity model_call_identity_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER model_call_identity_is_append_only BEFORE DELETE OR UPDATE ON model_call_identity FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: model_call model_call_instruction_manifest_is_immutable; Type: TRIGGER; Schema: public
--

CREATE TRIGGER model_call_instruction_manifest_is_immutable BEFORE UPDATE ON model_call FOR EACH ROW EXECUTE FUNCTION reject_model_call_instruction_manifest_change();


--
-- Name: model_call model_call_projects_terminal_usage; Type: TRIGGER; Schema: public
--

CREATE TRIGGER model_call_projects_terminal_usage AFTER INSERT OR UPDATE ON model_call FOR EACH ROW WHEN ((new.state_kind = 'terminal'::text)) EXECUTE FUNCTION project_terminal_model_call_usage();


--
-- Name: model_call model_call_requires_complete_final_state; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER model_call_requires_complete_final_state AFTER INSERT OR DELETE OR UPDATE ON model_call DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_model_call_final_state();


--
-- Name: model_call model_call_requires_failed_terminal_execution; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER model_call_requires_failed_terminal_execution AFTER INSERT OR DELETE OR UPDATE ON model_call DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_failed_terminal_execution_final_state();


--
-- Name: model_call model_call_reserves_global_identity; Type: TRIGGER; Schema: public
--

CREATE TRIGGER model_call_reserves_global_identity BEFORE INSERT ON model_call FOR EACH ROW EXECUTE FUNCTION reserve_model_call_identity('ordinary');


--
-- Name: model_call_transition_outbox_event model_call_transition_outbox_event_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER model_call_transition_outbox_event_cannot_be_truncated BEFORE TRUNCATE ON model_call_transition_outbox_event FOR EACH STATEMENT EXECUTE FUNCTION reject_outbox_table_truncate();


--
-- Name: model_call_transition_outbox_event model_call_transition_outbox_event_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER model_call_transition_outbox_event_is_append_only BEFORE DELETE OR UPDATE ON model_call_transition_outbox_event FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: model_call model_call_unsent_provider_failure_cause_is_absent; Type: TRIGGER; Schema: public
--

CREATE TRIGGER model_call_unsent_provider_failure_cause_is_absent BEFORE UPDATE ON model_call FOR EACH ROW EXECUTE FUNCTION reject_model_call_unsent_provider_failure_cause();


--
-- Name: model_call model_call_unsent_usage_is_unreported; Type: TRIGGER; Schema: public
--

CREATE TRIGGER model_call_unsent_usage_is_unreported BEFORE UPDATE ON model_call FOR EACH ROW EXECUTE FUNCTION reject_model_call_unsent_usage();


--
-- Name: model_call model_call_usage_metadata_is_immutable; Type: TRIGGER; Schema: public
--

CREATE TRIGGER model_call_usage_metadata_is_immutable BEFORE UPDATE ON model_call FOR EACH ROW EXECUTE FUNCTION reject_model_call_usage_metadata_rewrite();


--
-- Name: model_call_user_override model_call_user_override_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER model_call_user_override_is_append_only BEFORE DELETE OR UPDATE ON model_call_user_override FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: semantic_transcript_entry semantic_transcript_response_text_positions_contiguous; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER semantic_transcript_response_text_positions_contiguous AFTER INSERT ON semantic_transcript_entry DEFERRABLE INITIALLY DEFERRED FOR EACH ROW WHEN ((new.payload_kind = 'assistant_text'::text)) EXECUTE FUNCTION require_contiguous_assistant_response_text_positions();


--
-- Foreign keys.
--

--
-- Name: context_compacted_outbox_event context_compacted_outbox_call_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY context_compacted_outbox_event
    ADD CONSTRAINT context_compacted_outbox_call_fk FOREIGN KEY (model_call_id, session_id) REFERENCES context_compaction_model_call(model_call_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: context_compacted_outbox_event context_compacted_outbox_compaction_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY context_compacted_outbox_event
    ADD CONSTRAINT context_compacted_outbox_compaction_fk FOREIGN KEY (context_compaction_id, session_id) REFERENCES context_compaction(context_compaction_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: context_compacted_outbox_event context_compacted_outbox_frontier_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY context_compacted_outbox_event
    ADD CONSTRAINT context_compacted_outbox_frontier_fk FOREIGN KEY (session_id, result_frontier_id) REFERENCES context_frontier(owning_session_id, context_frontier_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: context_compacted_outbox_event context_compacted_outbox_header_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY context_compacted_outbox_event
    ADD CONSTRAINT context_compacted_outbox_header_fk FOREIGN KEY (event_sequence, event_kind, storage_version, session_id) REFERENCES outbox_event(event_sequence, event_kind, storage_version, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: context_compacted_outbox_event context_compacted_outbox_summary_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY context_compacted_outbox_event
    ADD CONSTRAINT context_compacted_outbox_summary_fk FOREIGN KEY (session_id, summary_entry_id) REFERENCES semantic_transcript_entry(source_session_id, semantic_entry_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: context_compaction context_compaction_call_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY context_compaction
    ADD CONSTRAINT context_compaction_call_fk FOREIGN KEY (producing_call_id, session_id) REFERENCES context_compaction_model_call(model_call_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: context_compaction context_compaction_first_entry_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY context_compaction
    ADD CONSTRAINT context_compaction_first_entry_fk FOREIGN KEY (first_source_session_id, first_entry_id) REFERENCES semantic_transcript_entry(source_session_id, semantic_entry_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: context_compaction_model_call context_compaction_model_call_frontier_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY context_compaction_model_call
    ADD CONSTRAINT context_compaction_model_call_frontier_fk FOREIGN KEY (session_id, source_frontier_id) REFERENCES context_frontier(owning_session_id, context_frontier_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: context_compaction_model_call context_compaction_model_call_session_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY context_compaction_model_call
    ADD CONSTRAINT context_compaction_model_call_session_fk FOREIGN KEY (session_id) REFERENCES session(session_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: context_compaction context_compaction_predecessor_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY context_compaction
    ADD CONSTRAINT context_compaction_predecessor_fk FOREIGN KEY (predecessor_compaction_id, session_id) REFERENCES context_compaction(context_compaction_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: context_compaction context_compaction_result_frontier_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY context_compaction
    ADD CONSTRAINT context_compaction_result_frontier_fk FOREIGN KEY (session_id, result_frontier_id) REFERENCES context_frontier(owning_session_id, context_frontier_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: context_compaction context_compaction_session_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY context_compaction
    ADD CONSTRAINT context_compaction_session_fk FOREIGN KEY (session_id) REFERENCES session(session_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: context_compaction context_compaction_source_frontier_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY context_compaction
    ADD CONSTRAINT context_compaction_source_frontier_fk FOREIGN KEY (session_id, source_frontier_id) REFERENCES context_frontier(owning_session_id, context_frontier_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: context_compaction context_compaction_summary_entry_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY context_compaction
    ADD CONSTRAINT context_compaction_summary_entry_fk FOREIGN KEY (session_id, summary_entry_id) REFERENCES semantic_transcript_entry(source_session_id, semantic_entry_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: context_compaction context_compaction_through_entry_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY context_compaction
    ADD CONSTRAINT context_compaction_through_entry_fk FOREIGN KEY (through_source_session_id, through_entry_id) REFERENCES semantic_transcript_entry(source_session_id, semantic_entry_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: context_frontier_delta context_frontier_member_entry_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY context_frontier_delta
    ADD CONSTRAINT context_frontier_member_entry_fk FOREIGN KEY (source_session_id, semantic_entry_id) REFERENCES semantic_transcript_entry(source_session_id, semantic_entry_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: context_frontier_delta context_frontier_member_frontier_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY context_frontier_delta
    ADD CONSTRAINT context_frontier_member_frontier_fk FOREIGN KEY (owning_session_id, context_frontier_id) REFERENCES context_frontier(owning_session_id, context_frontier_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: context_frontier context_frontier_owning_session_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY context_frontier
    ADD CONSTRAINT context_frontier_owning_session_fk FOREIGN KEY (owning_session_id) REFERENCES session(session_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: context_frontier context_frontier_prefix_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY context_frontier
    ADD CONSTRAINT context_frontier_prefix_fk FOREIGN KEY (owning_session_id, prefix_context_frontier_id) REFERENCES context_frontier(owning_session_id, context_frontier_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: credential_pool_availability_successor credential_pool_availability_suc_predecessor_model_call_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY credential_pool_availability_successor
    ADD CONSTRAINT credential_pool_availability_suc_predecessor_model_call_id_fkey FOREIGN KEY (predecessor_model_call_id) REFERENCES model_call(model_call_id);


--
-- Name: credential_pool_availability_successor credential_pool_availability_suc_successor_turn_attempt_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY credential_pool_availability_successor
    ADD CONSTRAINT credential_pool_availability_suc_successor_turn_attempt_id_fkey FOREIGN KEY (successor_turn_attempt_id) REFERENCES turn_attempt(turn_attempt_id);


--
-- Name: credential_pool_chain_exclusion credential_pool_chain_exclusion_predecessor_model_call_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY credential_pool_chain_exclusion
    ADD CONSTRAINT credential_pool_chain_exclusion_predecessor_model_call_id_fkey FOREIGN KEY (predecessor_model_call_id) REFERENCES model_call(model_call_id);


--
-- Name: credential_pool_chain_exclusion credential_pool_chain_exclusion_session_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY credential_pool_chain_exclusion
    ADD CONSTRAINT credential_pool_chain_exclusion_session_id_fkey FOREIGN KEY (session_id) REFERENCES session(session_id);


--
-- Name: credential_pool_chain_exclusion credential_pool_chain_exclusion_turn_id_session_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY credential_pool_chain_exclusion
    ADD CONSTRAINT credential_pool_chain_exclusion_turn_id_session_id_fkey FOREIGN KEY (turn_id, session_id) REFERENCES turn_lifecycle(turn_id, session_id);


--
-- Name: credential_pool_member_action credential_pool_member_action_observation_model_call_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY credential_pool_member_action
    ADD CONSTRAINT credential_pool_member_action_observation_model_call_id_fkey FOREIGN KEY (observation_model_call_id) REFERENCES model_call(model_call_id);


--
-- Name: credential_pool_member_action credential_pool_member_action_observed_session_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY credential_pool_member_action
    ADD CONSTRAINT credential_pool_member_action_observed_session_id_fkey FOREIGN KEY (observed_session_id) REFERENCES session(session_id);


--
-- Name: credential_pool_member_action credential_pool_member_action_observed_turn_id_observed_se_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY credential_pool_member_action
    ADD CONSTRAINT credential_pool_member_action_observed_turn_id_observed_se_fkey FOREIGN KEY (observed_turn_id, observed_session_id) REFERENCES turn_lifecycle(turn_id, session_id);


--
-- Name: credential_pool_terminal_exhaustion credential_pool_terminal_exhaustion_session_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY credential_pool_terminal_exhaustion
    ADD CONSTRAINT credential_pool_terminal_exhaustion_session_id_fkey FOREIGN KEY (session_id) REFERENCES session(session_id);


--
-- Name: credential_pool_terminal_exhaustion credential_pool_terminal_exhaustion_terminal_attempt_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY credential_pool_terminal_exhaustion
    ADD CONSTRAINT credential_pool_terminal_exhaustion_terminal_attempt_id_fkey FOREIGN KEY (terminal_attempt_id) REFERENCES turn_attempt(turn_attempt_id);


--
-- Name: credential_pool_terminal_exhaustion credential_pool_terminal_exhaustion_terminal_model_call_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY credential_pool_terminal_exhaustion
    ADD CONSTRAINT credential_pool_terminal_exhaustion_terminal_model_call_id_fkey FOREIGN KEY (terminal_model_call_id) REFERENCES model_call(model_call_id);


--
-- Name: credential_pool_terminal_exhaustion credential_pool_terminal_exhaustion_turn_id_session_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY credential_pool_terminal_exhaustion
    ADD CONSTRAINT credential_pool_terminal_exhaustion_turn_id_session_id_fkey FOREIGN KEY (turn_id, session_id) REFERENCES turn_lifecycle(turn_id, session_id);


--
-- Name: model_call model_call_attempt_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY model_call
    ADD CONSTRAINT model_call_attempt_fk FOREIGN KEY (turn_attempt_id, turn_id, session_id) REFERENCES turn_attempt(turn_attempt_id, turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: model_call_credential_pool_member model_call_credential_pool_member_model_call_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY model_call_credential_pool_member
    ADD CONSTRAINT model_call_credential_pool_member_model_call_id_fkey FOREIGN KEY (model_call_id) REFERENCES model_call_credential_pool_policy(model_call_id);


--
-- Name: model_call_credential_pool_policy model_call_credential_pool_policy_model_call_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY model_call_credential_pool_policy
    ADD CONSTRAINT model_call_credential_pool_policy_model_call_id_fkey FOREIGN KEY (model_call_id) REFERENCES model_call(model_call_id);


--
-- Name: model_call model_call_frontier_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY model_call
    ADD CONSTRAINT model_call_frontier_fk FOREIGN KEY (session_id, context_frontier_id) REFERENCES context_frontier(owning_session_id, context_frontier_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: model_call model_call_pinned_target_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY model_call
    ADD CONSTRAINT model_call_pinned_target_fk FOREIGN KEY (turn_id, session_id, resolved_provider_model_identity_id) REFERENCES turn_lifecycle(turn_id, session_id, pinned_provider_model_identity_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: model_call_transition_outbox_event model_call_transition_outbox_call_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY model_call_transition_outbox_event
    ADD CONSTRAINT model_call_transition_outbox_call_fk FOREIGN KEY (model_call_id, turn_id, session_id) REFERENCES model_call(model_call_id, turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: model_call_transition_outbox_event model_call_transition_outbox_header_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY model_call_transition_outbox_event
    ADD CONSTRAINT model_call_transition_outbox_header_fk FOREIGN KEY (event_sequence, event_kind, storage_version, session_id) REFERENCES outbox_event(event_sequence, event_kind, storage_version, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: model_call_user_override model_call_user_override_call_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY model_call_user_override
    ADD CONSTRAINT model_call_user_override_call_fk FOREIGN KEY (model_call_id) REFERENCES model_call(model_call_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


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
    -- the canonical restore-safe pin: the migration-selected schema, then pg_catalog, then pg_temp
    FOREACH signature IN ARRAY ARRAY[
        'context_frontier_member_position(uuid, uuid, uuid, uuid)',
        'context_frontier_preserves_prefix(uuid, uuid, uuid)',
        'require_context_compaction_exact_evidence()'
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
