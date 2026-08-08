-- Store the runner-loss wait as a closed, relationally authenticated active
-- turn phase without opening a generic producer for the loss transition.

ALTER TABLE runner_session_placement_record
    ADD COLUMN interrupted_tool_attempt_id uuid,
    ADD CONSTRAINT runner_session_placement_interrupted_attempt_shape CHECK (
        interrupted_tool_attempt_id IS NULL OR event_kind = 'runner_lost'
    ),
    ADD CONSTRAINT runner_session_placement_interrupted_attempt_fk
        FOREIGN KEY (interrupted_tool_attempt_id, session_id)
        REFERENCES tool_attempt (attempt_id, session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED;

ALTER TABLE turn_lifecycle
    ADD COLUMN runner_recovery_runner_id uuid,
    ADD COLUMN runner_recovery_placement_revision numeric(20, 0),
    ADD COLUMN runner_recovery_tool_attempt_id uuid,
    DROP CONSTRAINT turn_lifecycle_active_phase_closed;

ALTER TABLE turn_lifecycle
    ADD CONSTRAINT turn_lifecycle_active_phase_closed CHECK (
        active_phase_kind IS NULL OR active_phase_kind IN (
            'running', 'awaiting_model_call_recovery',
            'awaiting_tool_approval', 'awaiting_child',
            'awaiting_tool_recovery', 'awaiting_runner_recovery'
        )
    ),
    ADD CONSTRAINT turn_lifecycle_runner_recovery_revision_positive CHECK (
        runner_recovery_placement_revision IS NULL OR
        runner_recovery_placement_revision BETWEEN 1 AND 18446744073709551615
    ),
    ADD CONSTRAINT turn_lifecycle_runner_recovery_tool_attempt_fk
        FOREIGN KEY (runner_recovery_tool_attempt_id, session_id)
        REFERENCES tool_attempt (attempt_id, session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED;

DO $migration$
DECLARE legacy_shape text;
BEGIN
    SELECT pg_get_expr(conbin, conrelid) INTO legacy_shape
      FROM pg_constraint
     WHERE conrelid = 'turn_lifecycle'::regclass
       AND conname = 'turn_lifecycle_state_payload_shape';
    IF legacy_shape IS NULL THEN
        RAISE EXCEPTION 'turn-lifecycle legacy payload shape is missing';
    END IF;
    ALTER TABLE turn_lifecycle
        DROP CONSTRAINT turn_lifecycle_state_payload_shape;
    EXECUTE format(
        'ALTER TABLE turn_lifecycle
         ADD CONSTRAINT turn_lifecycle_state_payload_shape CHECK (
            ((%s)
                AND runner_recovery_runner_id IS NULL
                AND runner_recovery_placement_revision IS NULL
                AND runner_recovery_tool_attempt_id IS NULL)
            OR (
                state_kind = ''active''
                AND start_lineage_kind IS NOT NULL
                AND starting_frontier_id IS NOT NULL
                AND terminal_frontier_id IS NULL
                AND active_phase_kind = ''awaiting_runner_recovery''
                AND current_attempt_id IS NULL
                AND terminal_disposition_kind IS NULL
                AND recovery_model_call_id IS NULL
                AND approval_tool_request_id IS NULL
                AND recovery_tool_attempt_id IS NULL
                AND child_wait_request_id IS NULL
                AND terminal_attempt_id IS NULL
                AND terminal_model_call_id IS NULL
                AND terminal_tool_attempt_id IS NULL
                AND runner_recovery_runner_id IS NOT NULL
                AND runner_recovery_placement_revision IS NOT NULL
            )
         )',
        legacy_shape
    );
END;
$migration$;

CREATE FUNCTION assert_turn_runner_recovery_complete(
    checked_session_id uuid,
    checked_turn_id uuid
)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    lifecycle turn_lifecycle%ROWTYPE;
    placement runner_session_placement_record%ROWTYPE;
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
    IF lifecycle.runner_recovery_tool_attempt_id IS NOT NULL
       AND NOT EXISTS (
            SELECT 1
              FROM tool_attempt AS attempt
              JOIN tool_request AS request
                ON request.request_id = attempt.request_id
               AND request.turn_id = attempt.turn_id
               AND request.session_id = attempt.session_id
             WHERE attempt.attempt_id =
                    lifecycle.runner_recovery_tool_attempt_id
               AND attempt.turn_id = checked_turn_id
               AND attempt.session_id = checked_session_id
               AND request.producing_model_call_id =
                    lifecycle.active_tool_round_call_id
       )
    THEN
        RAISE EXCEPTION
            'runner recovery tool attempt lacks its exact active tool round'
            USING ERRCODE = '23514';
    END IF;
END;
$$;

CREATE FUNCTION require_turn_runner_recovery_complete()
RETURNS trigger
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

CREATE CONSTRAINT TRIGGER turn_lifecycle_runner_recovery_is_complete
AFTER INSERT OR UPDATE OR DELETE ON turn_lifecycle
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_turn_runner_recovery_complete();

CREATE FUNCTION recheck_session_turn_runner_recovery()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    checked_session_id uuid := COALESCE(NEW.session_id, OLD.session_id);
    checked_turn_id uuid;
BEGIN
    FOR checked_turn_id IN
        SELECT turn_id
          FROM turn_lifecycle
         WHERE session_id = checked_session_id
           AND active_phase_kind = 'awaiting_runner_recovery'
    LOOP
        PERFORM assert_turn_runner_recovery_complete(
            checked_session_id,
            checked_turn_id
        );
    END LOOP;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER runner_placement_rechecks_turn_recovery
AFTER INSERT OR UPDATE OR DELETE ON runner_current_session_placement
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION recheck_session_turn_runner_recovery();

CREATE FUNCTION reject_runner_recovery_reopen()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF OLD.state_kind = 'active'
       AND OLD.active_phase_kind = 'awaiting_runner_recovery'
       AND NEW.state_kind = 'active'
    THEN
        RAISE EXCEPTION
            'runner recovery wait cannot reopen without a checked replacement'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER turn_lifecycle_runner_recovery_does_not_reopen
BEFORE UPDATE ON turn_lifecycle
FOR EACH ROW
EXECUTE FUNCTION reject_runner_recovery_reopen();

-- The baseline lifecycle checker formerly divided active turns into only a
-- running arm and a model-call-recovery arm. Runner recovery deliberately has
-- no current turn attempt, while retaining any ended attempt history.
DO $migration$
DECLARE
    current_definition text;
    replacement_definition text;
    old_arm text := $old$
        ELSIF checked_active_phase = 'awaiting_child' THEN
            IF live_attempt_count <> 0 OR exact_attempt_count <> 0 THEN
                RAISE EXCEPTION 'child-wait turn % retains a live current attempt', checked_turn_id
                    USING ERRCODE = '23514';
            END IF;
        ELSE
$old$;
    new_arm text := $new$
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
$new$;
BEGIN
    SELECT pg_get_functiondef(
        'assert_turn_lifecycle_final_state_without_steering(uuid)'::regprocedure
    ) INTO current_definition;
    replacement_definition := replace(current_definition, old_arm, new_arm);
    IF replacement_definition = current_definition THEN
        RAISE EXCEPTION
            'turn-lifecycle runner-recovery insertion point is missing';
    END IF;
    EXECUTE replacement_definition;
END;
$migration$;

-- The tool-loop final-state checker predates the runner-recovery phase. Keep
-- its complete current definition and add only the new no-live-attempt arm;
-- the exact placement and optional tool-attempt correlation remains owned by
-- the dedicated deferred constraint above.
DO $migration$
DECLARE
    current_definition text;
    replacement_definition text;
    old_arm text := $old$
            ELSE
                RAISE EXCEPTION 'unsupported active tool-loop phase'
                    USING ERRCODE = '23514';
$old$;
    new_arm text := $new$
            WHEN 'awaiting_runner_recovery' THEN
                IF live_attempt_count <> 0 THEN
                    RAISE EXCEPTION
                        'runner recovery wait retains a live turn attempt'
                        USING ERRCODE = '23514';
                END IF;
            ELSE
                RAISE EXCEPTION 'unsupported active tool-loop phase'
                    USING ERRCODE = '23514';
$new$;
BEGIN
    SELECT pg_get_functiondef(
        'assert_tool_loop_turn_final_state_pre_delegation(uuid)'::regprocedure
    ) INTO current_definition;
    replacement_definition := replace(current_definition, old_arm, new_arm);
    IF replacement_definition = current_definition THEN
        RAISE EXCEPTION
            'tool-loop final-state runner-recovery insertion point is missing';
    END IF;
    EXECUTE replacement_definition;
END;
$migration$;

-- Interrupt is not authority to consume an administrative runner-recovery
-- wait. Store that refusal as a distinct closed result and correlate it with
-- the exact active phase at commit.
DO $migration$
DECLARE
    closed_kind text;
    result_shape text;
BEGIN
    SELECT pg_get_expr(conbin, conrelid) INTO closed_kind
      FROM pg_constraint
     WHERE conrelid = 'submit_input_command'::regclass
       AND conname = 'submit_input_command_rejection_kind_closed';
    SELECT pg_get_expr(conbin, conrelid) INTO result_shape
      FROM pg_constraint
     WHERE conrelid = 'submit_input_command'::regclass
       AND conname = 'submit_input_command_result_shape';
    IF closed_kind IS NULL OR result_shape IS NULL THEN
        RAISE EXCEPTION 'submit-input result constraints are missing';
    END IF;
    ALTER TABLE submit_input_command
        DROP CONSTRAINT submit_input_command_rejection_kind_closed,
        DROP CONSTRAINT submit_input_command_result_shape;
    EXECUTE format(
        'ALTER TABLE submit_input_command
         ADD CONSTRAINT submit_input_command_rejection_kind_closed CHECK (
            (%s) OR rejection_kind =
                ''interrupt_unavailable_while_awaiting_runner_recovery''
         ),
         ADD CONSTRAINT submit_input_command_result_shape CHECK (
            (%s) OR (
                result_kind = ''rejected''
                AND rejection_kind =
                    ''interrupt_unavailable_while_awaiting_runner_recovery''
                AND delivery_kind = ''interrupt''
                AND result_accepted_input_id IS NULL
                AND result_turn_id IS NULL
                AND result_actual_active_turn_id = expected_active_turn_id
                AND result_actual_active_turn_id IS NOT NULL
                AND result_expected_active_turn_id IS NULL
                AND result_expected_defaults_version IS NULL
                AND result_current_defaults_version IS NULL
                AND result_unknown_alias_id IS NULL
                AND result_selected_defaults_version IS NULL
                AND result_last_position IS NULL
                AND result_existing_interrupt_command_id IS NULL
            )
         )',
        closed_kind,
        result_shape
    );
END;
$migration$;

DO $migration$
DECLARE
    current_definition text;
    replacement_definition text;
    old_arm text := $old$
    ELSIF NEW.rejection_kind
        = 'interrupt_unavailable_while_awaiting_approval'
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
$old$;
    new_arm text := $new$
    ELSIF NEW.rejection_kind IN (
        'interrupt_unavailable_while_awaiting_approval',
        'interrupt_unavailable_while_awaiting_runner_recovery'
    )
    THEN
        SELECT count(*)
          INTO matching_records
          FROM turn_lifecycle AS parked
         WHERE parked.turn_id = NEW.result_actual_active_turn_id
           AND parked.session_id = NEW.result_session_id
           AND parked.state_kind = 'active'
           AND parked.active_phase_kind = CASE NEW.rejection_kind
                WHEN 'interrupt_unavailable_while_awaiting_approval'
                    THEN 'awaiting_tool_approval'
                ELSE 'awaiting_runner_recovery'
           END
           AND NOT EXISTS (
                SELECT 1
                  FROM accepted_input
                 WHERE accepting_command_id = NEW.command_id
           );
$new$;
BEGIN
    SELECT pg_get_functiondef(
        'require_interrupt_submit_input_effect_correlation()'::regprocedure
    ) INTO current_definition;
    replacement_definition := replace(current_definition, old_arm, new_arm);
    IF replacement_definition = current_definition THEN
        RAISE EXCEPTION
            'submit-input runner-recovery rejection insertion point is missing';
    END IF;
    EXECUTE replacement_definition;
END;
$migration$;

DROP TRIGGER submit_input_command_requires_correlated_effect
    ON submit_input_command;
DROP TRIGGER submit_input_command_requires_interrupt_effect
    ON submit_input_command;

CREATE CONSTRAINT TRIGGER submit_input_command_requires_correlated_effect
AFTER INSERT ON submit_input_command
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
WHEN (
    NOT (
        (
            NEW.result_kind = 'applied'
            AND NEW.delivery_kind = 'interrupt'
        )
        OR COALESCE(
            NEW.rejection_kind IN (
                'safe_point_unavailable_while_stopping',
                'interrupt_already_applied',
                'interrupt_unavailable_while_awaiting_approval',
                'interrupt_unavailable_while_awaiting_runner_recovery'
            ),
            false
        )
    )
)
EXECUTE FUNCTION require_submit_input_legacy_effect_correlation();

CREATE CONSTRAINT TRIGGER submit_input_command_requires_interrupt_effect
AFTER INSERT ON submit_input_command
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
WHEN (
    (
        NEW.result_kind = 'applied'
        AND NEW.delivery_kind = 'interrupt'
    )
    OR COALESCE(
        NEW.rejection_kind IN (
            'safe_point_unavailable_while_stopping',
            'interrupt_already_applied',
            'interrupt_unavailable_while_awaiting_approval',
            'interrupt_unavailable_while_awaiting_runner_recovery'
        ),
        false
    )
)
EXECUTE FUNCTION require_interrupt_submit_input_effect_correlation();
