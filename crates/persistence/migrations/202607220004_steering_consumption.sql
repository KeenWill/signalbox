-- Atomic NextSafePoint steering consumption at model-call preparation.
--
-- A consumed receipt remains bound to its immutable source turn and accepting
-- command. The prepared call, semantic entries, exact extended frontier, and
-- receipt dispositions are one deferred-constraint-checked commit.

ALTER TABLE accepted_input
    ADD COLUMN consuming_model_call_id uuid,
    DROP CONSTRAINT accepted_input_delivery_shape,
    DROP CONSTRAINT accepted_input_disposition_closed;

ALTER TABLE accepted_input
    ADD CONSTRAINT accepted_input_delivery_shape
    CHECK (
        (
            disposition_kind = 'origin_of'
            AND delivery_kind IN (
                'start_when_no_active_turn',
                'after_current_turn'
            )
            AND (
                (
                    delivery_kind = 'start_when_no_active_turn'
                    AND expected_active_turn_id IS NULL
                )
                OR
                (
                    delivery_kind = 'after_current_turn'
                    AND expected_active_turn_id IS NOT NULL
                )
            )
            AND expected_defaults_version IS NOT NULL
            AND model_override_kind IS NOT NULL
            AND origin_turn_id IS NOT NULL
            AND consuming_model_call_id IS NULL
        )
        OR
        (
            disposition_kind IN (
                'pending_steering',
                'consumed_as_steering'
            )
            AND delivery_kind = 'next_safe_point'
            AND expected_active_turn_id IS NOT NULL
            AND expected_defaults_version IS NULL
            AND model_override_kind IS NULL
            AND replacement_model_kind IS NULL
            AND replacement_direct_model_selection_id IS NULL
            AND replacement_model_alias_id IS NULL
            AND origin_turn_id IS NULL
            AND (
                (
                    disposition_kind = 'pending_steering'
                    AND consuming_model_call_id IS NULL
                )
                OR
                (
                    disposition_kind = 'consumed_as_steering'
                    AND consuming_model_call_id IS NOT NULL
                )
            )
        )
        OR
        (
            disposition_kind = 'reclassified_as_turn_origin'
            AND delivery_kind = 'next_safe_point'
            AND expected_active_turn_id IS NOT NULL
            AND expected_defaults_version IS NULL
            AND model_override_kind IS NULL
            AND replacement_model_kind IS NULL
            AND replacement_direct_model_selection_id IS NULL
            AND replacement_model_alias_id IS NULL
            AND origin_turn_id IS NOT NULL
            AND consuming_model_call_id IS NULL
        )
    ),
    ADD CONSTRAINT accepted_input_disposition_closed
    CHECK (
        disposition_kind IN (
            'origin_of',
            'pending_steering',
            'consumed_as_steering',
            'reclassified_as_turn_origin'
        )
    ),
    ADD CONSTRAINT accepted_input_consuming_call_fk
        FOREIGN KEY (
            consuming_model_call_id,
            expected_active_turn_id,
            session_id
        )
        REFERENCES model_call (model_call_id, turn_id, session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED;

CREATE INDEX accepted_input_consumed_by_model_call
    ON accepted_input (session_id, consuming_model_call_id, acceptance_position)
    WHERE disposition_kind = 'consumed_as_steering';

CREATE OR REPLACE FUNCTION reject_invalid_accepted_input_change()
RETURNS trigger
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
            OLD.content_kind,
            OLD.content_text,
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
            NEW.content_kind,
            NEW.content_text,
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

    RAISE EXCEPTION 'accepted_input is immutable outside pending-steering disposition'
        USING ERRCODE = '23514';
END;
$$;

ALTER TABLE semantic_transcript_entry
    ADD COLUMN steering_source_turn_id uuid,
    DROP CONSTRAINT semantic_transcript_entry_payload_kind_closed,
    DROP CONSTRAINT semantic_transcript_entry_payload_shape;

ALTER TABLE semantic_transcript_entry
    ADD CONSTRAINT semantic_transcript_entry_payload_kind_closed
        CHECK (
            payload_kind IN (
                'origin_accepted_input',
                'steering_accepted_input',
                'turn_failed',
                'assistant_text',
                'assistant_tool_use',
                'turn_completed'
            )
        ),
    ADD CONSTRAINT semantic_transcript_entry_payload_shape
        CHECK (
            (
                payload_kind IN (
                    'origin_accepted_input',
                    'steering_accepted_input'
                )
                AND origin_accepted_input_id IS NOT NULL
                AND (
                    (
                        payload_kind = 'origin_accepted_input'
                        AND steering_source_turn_id IS NULL
                    )
                    OR
                    (
                        payload_kind = 'steering_accepted_input'
                        AND steering_source_turn_id IS NOT NULL
                    )
                )
                AND failed_turn_id IS NULL
                AND assistant_text_value IS NULL
                AND producing_model_call_id IS NULL
                AND assistant_tool_request_id IS NULL
                AND completed_turn_id IS NULL
            )
            OR
            (
                payload_kind = 'turn_failed'
                AND origin_accepted_input_id IS NULL
                AND steering_source_turn_id IS NULL
                AND failed_turn_id IS NOT NULL
                AND assistant_text_value IS NULL
                AND producing_model_call_id IS NULL
                AND assistant_tool_request_id IS NULL
                AND completed_turn_id IS NULL
            )
            OR
            (
                payload_kind = 'assistant_text'
                AND origin_accepted_input_id IS NULL
                AND steering_source_turn_id IS NULL
                AND failed_turn_id IS NULL
                AND assistant_text_value IS NOT NULL
                AND producing_model_call_id IS NOT NULL
                AND assistant_tool_request_id IS NULL
                AND completed_turn_id IS NULL
                AND assistant_text_value <> ''
            )
            OR
            (
                payload_kind = 'assistant_tool_use'
                AND origin_accepted_input_id IS NULL
                AND steering_source_turn_id IS NULL
                AND failed_turn_id IS NULL
                AND assistant_text_value IS NULL
                AND producing_model_call_id IS NOT NULL
                AND assistant_tool_request_id IS NOT NULL
                AND completed_turn_id IS NULL
            )
            OR
            (
                payload_kind = 'turn_completed'
                AND origin_accepted_input_id IS NULL
                AND steering_source_turn_id IS NULL
                AND failed_turn_id IS NULL
                AND assistant_text_value IS NULL
                AND producing_model_call_id IS NULL
                AND assistant_tool_request_id IS NULL
                AND completed_turn_id IS NOT NULL
            )
        ),
    ADD CONSTRAINT semantic_transcript_entry_steering_source_turn_fk
        FOREIGN KEY (steering_source_turn_id, source_session_id)
        REFERENCES turn_lifecycle (turn_id, session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED;

CREATE FUNCTION assert_model_call_steering_final_state(
    checked_model_call_id uuid
)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    checked_session uuid;
    checked_turn uuid;
    checked_frontier uuid;
    starting_frontier uuid;
    starting_count numeric(20, 0);
    checked_count numeric(20, 0);
    suffix_count bigint;
    consumed_count bigint;
    malformed_count bigint;
BEGIN
    SELECT
        call.session_id,
        call.turn_id,
        call.context_frontier_id,
        lifecycle.starting_frontier_id
      INTO
        checked_session,
        checked_turn,
        checked_frontier,
        starting_frontier
      FROM model_call AS call
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
       OR EXISTS (
            SELECT 1
              FROM context_frontier_member AS starting
              LEFT JOIN context_frontier_member AS checked
                ON checked.owning_session_id = starting.owning_session_id
               AND checked.context_frontier_id = checked_frontier
               AND checked.member_position = starting.member_position
             WHERE starting.owning_session_id = checked_session
               AND starting.context_frontier_id = starting_frontier
               AND ROW(
                    checked.source_session_id,
                    checked.semantic_entry_id
               ) IS DISTINCT FROM ROW(
                    starting.source_session_id,
                    starting.semantic_entry_id
               )
       )
    THEN
        RAISE EXCEPTION 'model call steering frontier does not preserve its starting prefix'
            USING ERRCODE = '23514';
    END IF;

    SELECT count(*)
      INTO suffix_count
      FROM context_frontier_member
     WHERE owning_session_id = checked_session
       AND context_frontier_id = checked_frontier
       AND member_position > starting_count;

    SELECT count(*)
      INTO consumed_count
      FROM accepted_input
     WHERE session_id = checked_session
       AND expected_active_turn_id = checked_turn
       AND disposition_kind = 'consumed_as_steering'
       AND consuming_model_call_id = checked_model_call_id;

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
       AND member.member_position > starting_count
       AND (
            entry.payload_kind IS DISTINCT FROM 'steering_accepted_input'
            OR entry.source_session_id IS DISTINCT FROM checked_session
            OR entry.steering_source_turn_id IS DISTINCT FROM checked_turn
            OR accepted.disposition_kind IS DISTINCT FROM 'consumed_as_steering'
            OR accepted.expected_active_turn_id IS DISTINCT FROM checked_turn
            OR accepted.consuming_model_call_id IS DISTINCT FROM checked_model_call_id
       );

    IF suffix_count IS DISTINCT FROM consumed_count
       OR malformed_count <> 0
       OR EXISTS (
            SELECT 1
              FROM accepted_input AS pending
              JOIN accepted_input AS consumed
                ON consumed.session_id = pending.session_id
               AND consumed.expected_active_turn_id
                   = pending.expected_active_turn_id
               AND consumed.disposition_kind = 'consumed_as_steering'
               AND consumed.consuming_model_call_id = checked_model_call_id
               AND consumed.acceptance_position > pending.acceptance_position
             WHERE pending.session_id = checked_session
               AND pending.expected_active_turn_id = checked_turn
               AND pending.disposition_kind = 'pending_steering'
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
                        ON entry.source_session_id = member.source_session_id
                       AND entry.semantic_entry_id = member.semantic_entry_id
                      JOIN accepted_input AS accepted
                        ON accepted.accepted_input_id = entry.origin_accepted_input_id
                       AND accepted.session_id = entry.source_session_id
                     WHERE member.owning_session_id = checked_session
                       AND member.context_frontier_id = checked_frontier
                       AND member.member_position > starting_count
              ) AS ordered
             WHERE ordered.acceptance_order <> ordered.member_order
       )
    THEN
        RAISE EXCEPTION 'model call steering suffix is not the exact accepted order'
            USING ERRCODE = '23514';
    END IF;
END;
$$;

CREATE OR REPLACE FUNCTION assert_model_call_final_state(
    checked_model_call_id uuid
)
RETURNS void
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

    WITH RECURSIVE configuration_origin AS (
        SELECT stored.*
          FROM queued_input_origin AS stored
         WHERE stored.turn_id = checked_turn_id
           AND stored.session_id = checked_session_id
        UNION
        SELECT source.*
          FROM configuration_origin AS current
          JOIN queued_input_origin AS source
            ON source.turn_id = current.source_configuration_turn_id
           AND source.session_id = current.session_id
    )
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
      JOIN configuration_origin AS origin
        ON origin.session_id = lifecycle.session_id
       AND origin.source_configuration_turn_id IS NULL
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
           OR attempt_state IS DISTINCT FROM 'prepared'
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

CREATE FUNCTION assert_steering_accepted_input_final_state(
    checked_accepted_input_id uuid
)
RETURNS void
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

CREATE FUNCTION require_accepted_input_steering_final_state()
RETURNS trigger
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

CREATE CONSTRAINT TRIGGER accepted_input_requires_steering_final_state
AFTER INSERT OR UPDATE OR DELETE ON accepted_input
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_accepted_input_steering_final_state();

CREATE FUNCTION require_semantic_steering_final_state()
RETURNS trigger
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

CREATE CONSTRAINT TRIGGER semantic_entry_requires_steering_final_state
AFTER INSERT OR UPDATE OR DELETE ON semantic_transcript_entry
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_semantic_steering_final_state();

CREATE OR REPLACE FUNCTION require_semantic_entry_turn_state()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    checked_payload_kind text;
    checked_source_session_id uuid;
    checked_origin_input_id uuid;
    checked_steering_source_turn_id uuid;
    checked_failed_turn_id uuid;
    checked_producing_call_id uuid;
    checked_completed_turn_id uuid;
    checked_turn_id uuid;
BEGIN
    IF TG_OP = 'DELETE' THEN
        checked_payload_kind := OLD.payload_kind;
        checked_source_session_id := OLD.source_session_id;
        checked_origin_input_id := OLD.origin_accepted_input_id;
        checked_steering_source_turn_id := OLD.steering_source_turn_id;
        checked_failed_turn_id := OLD.failed_turn_id;
        checked_producing_call_id := OLD.producing_model_call_id;
        checked_completed_turn_id := OLD.completed_turn_id;
    ELSE
        checked_payload_kind := NEW.payload_kind;
        checked_source_session_id := NEW.source_session_id;
        checked_origin_input_id := NEW.origin_accepted_input_id;
        checked_steering_source_turn_id := NEW.steering_source_turn_id;
        checked_failed_turn_id := NEW.failed_turn_id;
        checked_producing_call_id := NEW.producing_model_call_id;
        checked_completed_turn_id := NEW.completed_turn_id;
    END IF;

    CASE checked_payload_kind
        WHEN 'origin_accepted_input' THEN
            SELECT origin_turn_id
              INTO checked_turn_id
              FROM accepted_input
             WHERE accepted_input_id = checked_origin_input_id
               AND session_id = checked_source_session_id
               AND disposition_kind IN (
                    'origin_of',
                    'reclassified_as_turn_origin'
               )
               AND origin_turn_id IS NOT NULL;

            IF NOT FOUND THEN
                RAISE EXCEPTION 'semantic origin input % is not a turn origin', checked_origin_input_id
                    USING
                        ERRCODE = '23514',
                        CONSTRAINT = 'semantic_transcript_entry_origin_disposition';
            END IF;
        WHEN 'steering_accepted_input' THEN
            SELECT expected_active_turn_id, consuming_model_call_id
              INTO checked_turn_id, checked_producing_call_id
              FROM accepted_input
             WHERE accepted_input_id = checked_origin_input_id
               AND session_id = checked_source_session_id
               AND disposition_kind = 'consumed_as_steering'
               AND expected_active_turn_id = checked_steering_source_turn_id
               AND consuming_model_call_id IS NOT NULL;

            IF NOT FOUND THEN
                RAISE EXCEPTION 'semantic steering input is not consumed by its source turn call'
                    USING ERRCODE = '23514';
            END IF;
        WHEN 'turn_failed' THEN
            checked_turn_id := checked_failed_turn_id;
        WHEN 'turn_completed' THEN
            checked_turn_id := checked_completed_turn_id;
        WHEN 'assistant_text' THEN
            SELECT turn_id
              INTO checked_turn_id
              FROM model_call
             WHERE model_call_id = checked_producing_call_id
               AND state_kind = 'terminal'
               AND terminal_disposition_kind = 'completed';

            IF NOT FOUND THEN
                RAISE EXCEPTION 'assistant text requires its outcome-authoritative completed call'
                    USING ERRCODE = '23514';
            END IF;
        ELSE
            RAISE EXCEPTION 'semantic payload kind % lacks construction authority', checked_payload_kind
                USING ERRCODE = '23514';
    END CASE;

    PERFORM assert_turn_lifecycle_final_state(checked_turn_id);
    IF checked_producing_call_id IS NOT NULL THEN
        PERFORM assert_model_call_final_state(checked_producing_call_id);
    END IF;
    RETURN NULL;
END;
$$;

ALTER FUNCTION assert_turn_lifecycle_final_state(uuid)
    RENAME TO assert_turn_lifecycle_final_state_without_steering;

CREATE FUNCTION assert_steering_turn_terminal_final_state(
    checked_turn_id uuid
)
RETURNS void
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

CREATE FUNCTION assert_turn_lifecycle_final_state(
    checked_turn_id uuid
)
RETURNS void
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
