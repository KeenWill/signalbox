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

CREATE FUNCTION assert_runner_placement_interrupted_attempt_complete(
    checked_session_id uuid,
    checked_event_ordinal numeric
)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    placement runner_session_placement_record%ROWTYPE;
BEGIN
    SELECT * INTO placement
      FROM runner_session_placement_record
     WHERE session_id = checked_session_id
       AND event_ordinal = checked_event_ordinal;
    IF NOT FOUND OR placement.interrupted_tool_attempt_id IS NULL THEN
        RETURN;
    END IF;
    IF placement.event_kind <> 'runner_lost'
       OR NOT EXISTS (
            SELECT 1
              FROM tool_attempt AS attempt
              JOIN runner_current_tool_attempt AS current_attempt
                ON current_attempt.attempt_id = attempt.attempt_id
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
                    placement.interrupted_tool_attempt_id
               AND attempt.session_id = placement.session_id
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
                        AND (
                            lease.effect_class = 'side_effecting'
                            OR (
                                lease.effect_class = 'idempotent'
                                AND EXISTS (
                                    SELECT 1
                                      FROM turn_runner_recovery_interrupt_effect
                                        AS stopped_effect
                                      JOIN turn_lifecycle AS stopped_turn
                                        ON stopped_turn.session_id =
                                            stopped_effect.session_id
                                       AND stopped_turn.turn_id =
                                            stopped_effect.turn_id
                                     WHERE stopped_effect.session_id =
                                            placement.session_id
                                       AND stopped_effect.interrupted_tool_attempt_id =
                                            attempt.attempt_id
                                       AND stopped_turn.state_kind = 'terminal'
                                       AND stopped_turn.terminal_disposition_kind =
                                            'reconciliation_required'
                                       AND stopped_turn.terminal_tool_attempt_id =
                                            attempt.attempt_id
                                )
                            )
                        )
                    )
                    OR (
                        attempt.state_kind = 'terminal'
                        AND attempt.terminal_disposition_kind = 'known_failed'
                        AND attempt.error_kind = 'crash_lost'
                        AND attempt.error_detail IS NULL
                        AND (
                            lease_event.state_kind = 'lost_unclaimed'
                            OR (
                                lease_event.state_kind IN (
                                    'lost_execution_possible',
                                    'lost_claimed'
                                )
                                AND lease.effect_class = 'pure'
                            )
                        )
                    )
               )
               AND lease.runner_id = placement.lost_runner_id
               AND leased_placement.event_ordinal < placement.event_ordinal
               AND leased_placement.placement_revision =
                    placement.placement_revision
               AND leased_placement.state_kind = 'pinned'
               AND leased_placement.pinned_runner_id =
                    placement.lost_runner_id
               AND EXISTS (
                    SELECT 1
                      FROM turn_lifecycle AS lifecycle
                     WHERE lifecycle.turn_id = attempt.turn_id
                       AND lifecycle.session_id = attempt.session_id
                       AND lifecycle.state_kind = 'active'
                       AND lifecycle.active_phase_kind =
                            'awaiting_runner_recovery'
                       AND lifecycle.current_attempt_id IS NULL
                       AND lifecycle.active_tool_round_call_id =
                            request.producing_model_call_id
                       AND lifecycle.runner_recovery_runner_id =
                            placement.lost_runner_id
                       AND lifecycle.runner_recovery_placement_revision =
                            placement.placement_revision
                       AND lifecycle.runner_recovery_tool_attempt_id =
                            attempt.attempt_id
               )
       )
    THEN
        RAISE EXCEPTION
            'runner placement loss lacks exact interrupted lease lineage'
            USING ERRCODE = '23514';
    END IF;
END;
$$;

CREATE FUNCTION require_runner_placement_interrupted_attempt_complete()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    PERFORM assert_runner_placement_interrupted_attempt_complete(
        NEW.session_id,
        NEW.event_ordinal
    );
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER runner_placement_interrupted_attempt_is_complete
AFTER INSERT ON runner_session_placement_record
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_runner_placement_interrupted_attempt_complete();

-- A stop may retire a no-execution external-effect attempt as crash-lost.
-- The baseline attempt guard rejects that terminal evidence because it has no
-- runner-lease context; admit only the exact authenticated lost-unclaimed wait.
DO $migration$
DECLARE
    current_definition text;
    replacement_definition text;
    old_guard text := $old$
            OR (
                OLD.effect_class = 'external_effect'
                AND NEW.error_kind = 'crash_lost'
            )
$old$;
    new_guard text := $new$
            OR (
                OLD.effect_class = 'external_effect'
                AND NEW.error_kind = 'crash_lost'
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
$new$;
BEGIN
    SELECT pg_get_functiondef(
        'reject_tool_attempt_invalid_change()'::regprocedure
    ) INTO current_definition;
    replacement_definition := replace(
        current_definition,
        old_guard,
        new_guard
    );
    IF replacement_definition = current_definition THEN
        RAISE EXCEPTION
            'runner-recovery crash-loss insertion point is missing';
    END IF;
    EXECUTE replacement_definition;
END;
$migration$;

ALTER TABLE turn_lifecycle
    ADD COLUMN runner_recovery_runner_id uuid,
    ADD COLUMN runner_recovery_placement_revision numeric(20, 0),
    ADD COLUMN runner_recovery_tool_attempt_id uuid,
    -- Supersedes 202608020018_session_delegation.sql.
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
        -- Supersedes 202608020018_session_delegation.sql.
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
               AND leased_placement.placement_revision =
                    lifecycle.runner_recovery_placement_revision
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

CREATE CONSTRAINT TRIGGER tool_attempt_rechecks_turn_runner_recovery
AFTER INSERT OR UPDATE OR DELETE ON tool_attempt
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_turn_runner_recovery_complete();

CREATE CONSTRAINT TRIGGER turn_attempt_rechecks_turn_runner_recovery
AFTER INSERT OR UPDATE OR DELETE ON turn_attempt
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_turn_runner_recovery_complete();

CREATE CONSTRAINT TRIGGER tool_round_rechecks_turn_runner_recovery
AFTER INSERT OR UPDATE OR DELETE ON tool_round
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_turn_runner_recovery_complete();

CREATE FUNCTION lock_scheduler_before_runner_recovery_dependency_insert()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    PERFORM 1
      FROM session_scheduler
     WHERE session_id = NEW.session_id
     FOR UPDATE;
    IF NOT FOUND AND TG_TABLE_NAME = 'turn_attempt' THEN
        RAISE EXCEPTION 'turn attempt lacks its session scheduler'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER turn_attempt_00_locks_scheduler_before_insert
BEFORE INSERT ON turn_attempt
FOR EACH ROW
EXECUTE FUNCTION lock_scheduler_before_runner_recovery_dependency_insert();

CREATE TRIGGER tool_round_00_locks_scheduler_before_insert
BEFORE INSERT ON tool_round
FOR EACH ROW
EXECUTE FUNCTION lock_scheduler_before_runner_recovery_dependency_insert();

CREATE FUNCTION recheck_session_turn_runner_recovery()
RETURNS trigger
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

CREATE CONSTRAINT TRIGGER runner_placement_rechecks_turn_recovery
AFTER INSERT OR UPDATE OR DELETE ON runner_current_session_placement
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION recheck_session_turn_runner_recovery();

CREATE CONSTRAINT TRIGGER runner_lease_event_rechecks_turn_recovery
AFTER INSERT OR UPDATE OR DELETE ON runner_lease_event
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION recheck_session_turn_runner_recovery();

CREATE CONSTRAINT TRIGGER runner_current_lease_event_rechecks_turn_recovery
AFTER INSERT OR UPDATE OR DELETE ON runner_current_lease_event
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION recheck_session_turn_runner_recovery();

CREATE TABLE turn_runner_recovery_interrupt_effect (
    command_id uuid PRIMARY KEY,
    session_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    placement_event_ordinal numeric(20, 0) NOT NULL CHECK (
        placement_event_ordinal BETWEEN 1 AND 18446744073709551615
    ),
    runner_id uuid NOT NULL,
    placement_revision numeric(20, 0) NOT NULL CHECK (
        placement_revision BETWEEN 1 AND 18446744073709551615
    ),
    yielded_turn_attempt_id uuid NOT NULL,
    interrupted_tool_attempt_id uuid,
    source_frontier_id uuid NOT NULL,
    UNIQUE (session_id, turn_id),
    FOREIGN KEY (command_id) REFERENCES submit_input_command(command_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (turn_id, session_id) REFERENCES turn_lifecycle(turn_id, session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (yielded_turn_attempt_id, turn_id, session_id)
        REFERENCES turn_attempt(turn_attempt_id, turn_id, session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (session_id, placement_event_ordinal)
        REFERENCES runner_session_placement_record(session_id, event_ordinal)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (session_id, source_frontier_id)
        REFERENCES context_frontier(owning_session_id, context_frontier_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (interrupted_tool_attempt_id, session_id)
        REFERENCES tool_attempt(attempt_id, session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED
);

CREATE FUNCTION guard_turn_runner_recovery_interrupt_effect()
RETURNS trigger
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

CREATE TRIGGER turn_runner_recovery_interrupt_effect_is_authorized
BEFORE INSERT ON turn_runner_recovery_interrupt_effect
FOR EACH ROW
EXECUTE FUNCTION guard_turn_runner_recovery_interrupt_effect();

CREATE TRIGGER turn_runner_recovery_interrupt_effect_is_immutable
AFTER UPDATE OR DELETE ON turn_runner_recovery_interrupt_effect
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER turn_runner_recovery_interrupt_effect_rejects_truncate
BEFORE TRUNCATE ON turn_runner_recovery_interrupt_effect
FOR EACH STATEMENT
EXECUTE FUNCTION reject_immutable_record_change();

CREATE OR REPLACE FUNCTION require_interrupt_submit_input_effect_correlation()
RETURNS trigger
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
           AND accepted.content_kind = NEW.content_kind
           AND accepted.content_text = NEW.content_text
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
                           AND lease_event.event_ordinal =
                                lease_head.event_ordinal
                          JOIN runner_session_placement_record AS leased_placement
                            ON leased_placement.session_id = lease.session_id
                           AND leased_placement.event_ordinal =
                                lease.placement_event_ordinal
                         WHERE stopped_tool.attempt_id =
                                effect.interrupted_tool_attempt_id
                           AND stopped_tool.session_id = effect.session_id
                           AND stopped_tool.turn_id = effect.turn_id
                           AND stopped_tool.state_kind = 'terminal'
                           AND stopped_tool.terminal_disposition_kind =
                                'ambiguous'
                           AND lease.runner_id = effect.runner_id
                           AND lease_event.state_kind IN (
                                'lost_execution_possible', 'lost_claimed'
                           )
                           AND lease.effect_class IN (
                                'idempotent', 'side_effecting'
                           )
                           AND leased_placement.placement_revision =
                                effect.placement_revision
                           AND leased_placement.state_kind = 'pinned'
                           AND leased_placement.pinned_runner_id =
                                effect.runner_id
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
                           AND stopped_tool.terminal_disposition_kind =
                                'known_failed'
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
                    AND stopped_attempt.interrupt_predecessor_turn_id
                        = NEW.expected_active_turn_id
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
                           AND reconciled.terminal_disposition_kind
                               = 'reconciliation_required'
                           AND reconciled.terminal_attempt_id
                               = stopped_attempt.turn_attempt_id
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
                            ON producing_call.model_call_id
                                = awaiting.producing_model_call_id
                           AND producing_call.turn_id = awaiting.turn_id
                           AND producing_call.session_id = awaiting.session_id
                          JOIN turn_lifecycle AS cancelled
                            ON cancelled.turn_id = waiting.parent_turn_id
                           AND cancelled.session_id = waiting.parent_session_id
                         WHERE waiting.parent_turn_id = NEW.expected_active_turn_id
                           AND waiting.parent_session_id = NEW.session_id
                           AND waiting.wait_mode = 'foreground'
                           AND producing_call.turn_attempt_id
                               = stopped_attempt.turn_attempt_id
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
           AND accepted.content_kind = NEW.content_kind
           AND accepted.content_text = NEW.content_text
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
           AND successor.interrupt_predecessor_turn_id
               = NEW.expected_active_turn_id
           AND successor.defaults_version = NEW.expected_defaults_version;
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
           AND successor.interrupt_predecessor_turn_id
               = NEW.result_actual_active_turn_id
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
           AND existing.expected_active_turn_id
               = NEW.result_actual_active_turn_id
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

CREATE OR REPLACE FUNCTION assert_cancelled_turn_final_state(
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
            AND EXISTS (
                SELECT 1
                  FROM turn_attempt AS yielded_attempt
                 WHERE yielded_attempt.turn_attempt_id = OLD.current_attempt_id
                   AND yielded_attempt.turn_id = OLD.turn_id
                   AND yielded_attempt.session_id = OLD.session_id
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

-- A runner loss yields the issuing turn attempt before preserving an
-- execution-ambiguous physical attempt. Extend the existing reconciliation
-- checker only for that exact authenticated pair; retryable attempts stopped
-- as known crash loss use the cancellation checker above.
DO $migration$
DECLARE
    current_definition text;
    replacement_definition text;
    old_attempt_state text := $old$
           AND state_kind = 'ended'
           AND end_disposition IN ('ambiguous', 'lost')
           AND (
$old$;
    new_attempt_state text := $new$
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
$new$;
BEGIN
    SELECT pg_get_functiondef(
        'assert_reconciliation_required_turn_final_state(uuid)'::regprocedure
    ) INTO current_definition;
    replacement_definition := replace(
        current_definition,
        old_attempt_state,
        new_attempt_state
    );
    IF replacement_definition = current_definition THEN
        RAISE EXCEPTION
            'runner-recovery reconciliation insertion point is missing';
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
