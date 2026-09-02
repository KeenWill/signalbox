--
-- Session lifecycle §3: write-time stamps on the lifecycle rows.
--

ALTER TABLE outbox_event
    ADD COLUMN recorded_at timestamp with time zone
        DEFAULT statement_timestamp() NOT NULL;

ALTER TABLE delegation_outbox_event
    ADD COLUMN recorded_at timestamp with time zone
        DEFAULT statement_timestamp() NOT NULL;

ALTER TABLE session
    ADD COLUMN created_at timestamp with time zone
        DEFAULT statement_timestamp() NOT NULL;

ALTER TABLE turn_lifecycle
    ADD COLUMN recorded_at timestamp with time zone
        DEFAULT statement_timestamp() NOT NULL;

ALTER TABLE turn_attempt
    ADD COLUMN recorded_at timestamp with time zone
        DEFAULT statement_timestamp() NOT NULL;

ALTER TABLE model_call
    ADD COLUMN recorded_at timestamp with time zone
        DEFAULT statement_timestamp() NOT NULL;

ALTER TABLE tool_attempt
    ADD COLUMN recorded_at timestamp with time zone
        DEFAULT statement_timestamp() NOT NULL;

ALTER TABLE goal_event
    ADD COLUMN recorded_at timestamp with time zone
        DEFAULT statement_timestamp() NOT NULL;

--
-- `durable_command.claimed_at` never stands in for `requested_at`.
--

ALTER TABLE compact_session_command
    ADD COLUMN requested_at timestamp with time zone
        DEFAULT statement_timestamp() NOT NULL;

--
-- A prepared call may fail without ever going in flight.
--

ALTER TABLE context_compaction_model_call
    ADD COLUMN prepared_at timestamp with time zone
        DEFAULT statement_timestamp() NOT NULL,
    ADD COLUMN in_flight_at timestamp with time zone,
    ADD COLUMN terminal_at timestamp with time zone;

ALTER TABLE context_compaction_model_call
    ADD CONSTRAINT context_compaction_model_call_transition_stamps CHECK (
        ((state_kind <> 'in_flight'::text) OR (in_flight_at IS NOT NULL))
        AND ((state_kind = 'terminal'::text) = (terminal_at IS NOT NULL))
    );

ALTER TABLE context_compaction
    ADD COLUMN applied_at timestamp with time zone
        DEFAULT statement_timestamp() NOT NULL;

--
-- Stamps do not move. The append-only families already reject every update.
--

CREATE FUNCTION reject_recorded_at_change() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF OLD.recorded_at IS DISTINCT FROM NEW.recorded_at THEN
        RAISE EXCEPTION 'lifecycle row write time is immutable'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER turn_lifecycle_write_time_is_immutable BEFORE UPDATE ON turn_lifecycle FOR EACH ROW EXECUTE FUNCTION reject_recorded_at_change();

CREATE TRIGGER turn_attempt_write_time_is_immutable BEFORE UPDATE ON turn_attempt FOR EACH ROW EXECUTE FUNCTION reject_recorded_at_change();

CREATE TRIGGER model_call_write_time_is_immutable BEFORE UPDATE ON model_call FOR EACH ROW EXECUTE FUNCTION reject_recorded_at_change();

CREATE TRIGGER tool_attempt_write_time_is_immutable BEFORE UPDATE ON tool_attempt FOR EACH ROW EXECUTE FUNCTION reject_recorded_at_change();

CREATE FUNCTION reject_requested_at_change() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF OLD.requested_at IS DISTINCT FROM NEW.requested_at THEN
        RAISE EXCEPTION 'compaction request time is immutable'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER compact_session_command_request_time_is_immutable BEFORE UPDATE ON compact_session_command FOR EACH ROW EXECUTE FUNCTION reject_requested_at_change();

--
-- Each transition stamp is written once, by the transition it records.
--

CREATE FUNCTION reject_context_compaction_stamp_change() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        IF NEW.in_flight_at IS NOT NULL OR NEW.terminal_at IS NOT NULL THEN
            RAISE EXCEPTION 'compaction call begins with only its preparation time'
                USING ERRCODE = '23514';
        END IF;
        RETURN NEW;
    END IF;
    IF OLD.prepared_at IS DISTINCT FROM NEW.prepared_at THEN
        RAISE EXCEPTION 'compaction call preparation time is immutable'
            USING ERRCODE = '23514';
    END IF;
    IF OLD.in_flight_at IS NOT NULL
       AND NEW.in_flight_at IS DISTINCT FROM OLD.in_flight_at
    THEN
        RAISE EXCEPTION 'compaction call in-flight time is write-once'
            USING ERRCODE = '23514';
    END IF;
    IF OLD.in_flight_at IS NULL
       AND NEW.in_flight_at IS NOT NULL
       AND NEW.state_kind <> 'in_flight'
    THEN
        RAISE EXCEPTION 'compaction call in-flight time is written by authorization'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER context_compaction_model_call_stamps_are_write_once BEFORE INSERT OR UPDATE ON context_compaction_model_call FOR EACH ROW EXECUTE FUNCTION reject_context_compaction_stamp_change();
