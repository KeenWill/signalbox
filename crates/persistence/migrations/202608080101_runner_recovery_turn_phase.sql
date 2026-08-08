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
    IF lifecycle.runner_recovery_tool_attempt_id IS NOT NULL
       AND NOT EXISTS (
            SELECT 1
              FROM tool_attempt AS attempt
              JOIN tool_request AS request
               ON request.request_id = attempt.request_id
               AND request.turn_id = attempt.turn_id
               AND request.session_id = attempt.session_id
              JOIN runner_physical_attempt_lease_binding AS binding
                ON binding.attempt_id = attempt.attempt_id
              JOIN runner_lease_generation AS lease
                ON lease.lease_id = binding.lease_id
               AND lease.attempt_id = attempt.attempt_id
               AND lease.session_id = attempt.session_id
              JOIN runner_session_placement_record AS leased_placement
                ON leased_placement.session_id = lease.session_id
               AND leased_placement.event_ordinal =
                    lease.placement_event_ordinal
             WHERE attempt.attempt_id =
                    lifecycle.runner_recovery_tool_attempt_id
               AND attempt.turn_id = checked_turn_id
               AND attempt.session_id = checked_session_id
               AND request.producing_model_call_id =
                    lifecycle.active_tool_round_call_id
               AND lease.runner_id = lifecycle.runner_recovery_runner_id
               AND leased_placement.placement_revision =
                    lifecycle.runner_recovery_placement_revision
               AND leased_placement.state_kind = 'pinned'
               AND leased_placement.pinned_runner_id =
                    lifecycle.runner_recovery_runner_id
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
    IF NEW.state_kind = 'active'
       AND NEW.active_phase_kind = 'awaiting_runner_recovery'
       AND OLD.active_phase_kind IS DISTINCT FROM 'awaiting_runner_recovery'
       AND NOT (
            OLD.state_kind = 'active'
            AND OLD.active_phase_kind = 'running'
       )
    THEN
        RAISE EXCEPTION
            'runner recovery wait requires an active runner boundary'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.state_kind = 'active'
       AND OLD.active_phase_kind = 'awaiting_runner_recovery'
       AND NEW.state_kind = 'active'
       AND NOT (
            NOT OLD.delegation_runtime_terminal
            AND NEW.delegation_runtime_terminal
            AND (to_jsonb(OLD) - 'delegation_runtime_terminal') =
                (to_jsonb(NEW) - 'delegation_runtime_terminal')
       )
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
