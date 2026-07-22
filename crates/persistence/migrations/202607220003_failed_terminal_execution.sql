-- Preserve complete failed-turn terminal execution provenance. Direct static
-- failures remain valid without an attempt; an execution-backed failure must
-- name its sole ended attempt and may name the terminal call from that exact
-- attempt. This forward migration fails closed rather than guessing between
-- multiple historical records.

ALTER TABLE turn_lifecycle
    DROP CONSTRAINT turn_lifecycle_state_payload_shape;

DO $$
BEGIN
    IF EXISTS (
        SELECT 1
          FROM turn_lifecycle AS lifecycle
         WHERE lifecycle.state_kind = 'terminal'
           AND lifecycle.terminal_disposition_kind = 'failed'
           AND (
                (SELECT count(*)
                   FROM turn_attempt AS attempt
                  WHERE attempt.turn_id = lifecycle.turn_id
                    AND attempt.session_id = lifecycle.session_id) > 1
                OR EXISTS (
                    SELECT 1
                      FROM turn_attempt AS attempt
                     WHERE attempt.turn_id = lifecycle.turn_id
                       AND attempt.session_id = lifecycle.session_id
                       AND (
                            attempt.state_kind <> 'ended'
                            OR attempt.end_variant <> 'without_stop'
                            OR attempt.end_disposition NOT IN ('known_failure', 'lost')
                       )
                )
                OR (SELECT count(*)
                      FROM model_call AS call
                     WHERE call.turn_id = lifecycle.turn_id
                       AND call.session_id = lifecycle.session_id) > 1
                OR EXISTS (
                    SELECT 1
                      FROM model_call AS call
                      LEFT JOIN turn_attempt AS attempt
                        ON attempt.turn_attempt_id = call.turn_attempt_id
                       AND attempt.turn_id = call.turn_id
                       AND attempt.session_id = call.session_id
                     WHERE call.turn_id = lifecycle.turn_id
                       AND call.session_id = lifecycle.session_id
                       AND (
                            attempt.turn_attempt_id IS NULL
                            OR call.state_kind <> 'terminal'
                            OR call.terminal_disposition_kind NOT IN (
                                'known_failed',
                                'cancelled'
                            )
                       )
                )
           )
    ) THEN
        RAISE EXCEPTION
            'cannot backfill ambiguous or invalid failed terminal execution provenance'
            USING
                ERRCODE = '23514',
                CONSTRAINT = 'turn_lifecycle_failed_execution_backfill';
    END IF;
END;
$$;

-- Terminal rows are normally immutable. The migration owns this one guarded
-- metadata backfill and restores the trigger before adding the new invariant.
ALTER TABLE turn_lifecycle
    DISABLE TRIGGER turn_lifecycle_changes_are_guarded;

UPDATE turn_lifecycle AS lifecycle
   SET terminal_attempt_id = (
            SELECT attempt.turn_attempt_id
              FROM turn_attempt AS attempt
             WHERE attempt.turn_id = lifecycle.turn_id
               AND attempt.session_id = lifecycle.session_id
       ),
       terminal_model_call_id = (
            SELECT call.model_call_id
              FROM model_call AS call
             WHERE call.turn_id = lifecycle.turn_id
               AND call.session_id = lifecycle.session_id
       )
 WHERE lifecycle.state_kind = 'terminal'
   AND lifecycle.terminal_disposition_kind = 'failed';

ALTER TABLE turn_lifecycle
    ENABLE TRIGGER turn_lifecycle_changes_are_guarded;

ALTER TABLE turn_lifecycle
    ADD CONSTRAINT turn_lifecycle_state_payload_shape
        CHECK (
            (
                state_kind = 'queued'
                AND start_lineage_kind IS NULL
                AND immediate_predecessor_turn_id IS NULL
                AND starting_frontier_id IS NULL
                AND terminal_frontier_id IS NULL
                AND active_phase_kind IS NULL
                AND current_attempt_id IS NULL
                AND terminal_disposition_kind IS NULL
                AND recovery_model_call_id IS NULL
                AND terminal_attempt_id IS NULL
                AND terminal_model_call_id IS NULL
            )
            OR
            (
                state_kind = 'active'
                AND start_lineage_kind IS NOT NULL
                AND starting_frontier_id IS NOT NULL
                AND terminal_frontier_id IS NULL
                AND active_phase_kind = 'running'
                AND current_attempt_id IS NOT NULL
                AND terminal_disposition_kind IS NULL
                AND recovery_model_call_id IS NULL
                AND terminal_attempt_id IS NULL
                AND terminal_model_call_id IS NULL
            )
            OR
            (
                state_kind = 'active'
                AND start_lineage_kind IS NOT NULL
                AND starting_frontier_id IS NOT NULL
                AND terminal_frontier_id IS NULL
                AND active_phase_kind = 'awaiting_model_call_recovery'
                AND current_attempt_id IS NOT NULL
                AND terminal_disposition_kind IS NULL
                AND recovery_model_call_id IS NOT NULL
                AND terminal_attempt_id IS NULL
                AND terminal_model_call_id IS NULL
            )
            OR
            (
                state_kind = 'terminal'
                AND start_lineage_kind IS NOT NULL
                AND starting_frontier_id IS NOT NULL
                AND terminal_frontier_id IS NOT NULL
                AND active_phase_kind IS NULL
                AND current_attempt_id IS NULL
                AND terminal_disposition_kind = 'failed'
                AND recovery_model_call_id IS NULL
                AND (
                    (
                        terminal_attempt_id IS NULL
                        AND terminal_model_call_id IS NULL
                    )
                    OR terminal_attempt_id IS NOT NULL
                )
            )
            OR
            (
                state_kind = 'terminal'
                AND start_lineage_kind IS NOT NULL
                AND starting_frontier_id IS NOT NULL
                AND terminal_frontier_id IS NOT NULL
                AND active_phase_kind IS NULL
                AND current_attempt_id IS NULL
                AND terminal_disposition_kind IN ('completed', 'refused')
                AND recovery_model_call_id IS NULL
                AND terminal_attempt_id IS NOT NULL
                AND terminal_model_call_id IS NOT NULL
            )
        ) NOT VALID;

ALTER TABLE turn_lifecycle
    VALIDATE CONSTRAINT turn_lifecycle_state_payload_shape;

CREATE FUNCTION assert_failed_terminal_execution_final_state(
    checked_turn_id uuid
)
RETURNS void
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

CREATE FUNCTION require_failed_terminal_execution_final_state()
RETURNS trigger
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

CREATE CONSTRAINT TRIGGER turn_lifecycle_requires_failed_terminal_execution
AFTER INSERT OR UPDATE OR DELETE ON turn_lifecycle
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_failed_terminal_execution_final_state();

CREATE CONSTRAINT TRIGGER turn_attempt_requires_failed_terminal_execution
AFTER INSERT OR UPDATE OR DELETE ON turn_attempt
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_failed_terminal_execution_final_state();

CREATE CONSTRAINT TRIGGER model_call_requires_failed_terminal_execution
AFTER INSERT OR UPDATE OR DELETE ON model_call
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_failed_terminal_execution_final_state();
