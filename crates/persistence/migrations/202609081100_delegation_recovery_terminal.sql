-- A logical delegation terminal preserves every physical recovery fact.
CREATE OR REPLACE FUNCTION reject_turn_lifecycle_invalid_change() RETURNS trigger
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
       AND NOT (
            NOT OLD.delegation_runtime_terminal
            AND NEW.delegation_runtime_terminal
            AND (to_jsonb(OLD) - 'delegation_runtime_terminal') =
                (to_jsonb(NEW) - 'delegation_runtime_terminal')
       )
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
