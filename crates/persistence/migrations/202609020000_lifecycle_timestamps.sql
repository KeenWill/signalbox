--
-- Session lifecycle §3: write-time stamps on the lifecycle rows.
--
-- Every column lands NOT NULL outright. The unconstrained dogfood-database
-- reset is ratified, so no row predates these columns and no backfill marker
-- or `CHECK ... NOT VALID` scaffolding is owed. `statement_timestamp()` is
-- stable across the rows one statement writes and advances between statements,
-- so a stamp records when its own write happened rather than when the enclosing
-- transaction opened, and rows written together never acquire an artificial
-- order. The repository writes never restate it, which is what makes the stamp
-- impossible for a new write path to skip.
--

--
-- Outbox headers. Both families carry the event's own record time.
--

ALTER TABLE outbox_event
    ADD COLUMN recorded_at timestamp with time zone
        DEFAULT statement_timestamp() NOT NULL;

ALTER TABLE delegation_outbox_event
    ADD COLUMN recorded_at timestamp with time zone
        DEFAULT statement_timestamp() NOT NULL;

--
-- Session creation. `session` is append-only, so the row's insert time is its
-- creation time. `ended_at` is the lifecycle satellite's, not this table's.
--

ALTER TABLE session
    ADD COLUMN created_at timestamp with time zone
        DEFAULT statement_timestamp() NOT NULL;

--
-- The five lifecycle rows §3 names. Each is stamped when the row is written;
-- the mutable state machines (`turn_lifecycle`, `turn_attempt`, `model_call`,
-- `tool_attempt`) keep their transition history in their own state columns, so
-- this records the row's origin instant and never moves.
--

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
-- Compaction lifecycle. §3 requires the command's acceptance to carry its own
-- durable `requested_at`: `durable_command.claimed_at` is non-semantic
-- operational metadata and never stands in for it.
--

ALTER TABLE compact_session_command
    ADD COLUMN requested_at timestamp with time zone
        DEFAULT statement_timestamp() NOT NULL;

--
-- The compaction call's three transitions. `prepared_at` is the insert stamp;
-- the repository sets the other two in the same statement that moves
-- `state_kind`, so a stamp cannot disagree with the state it records.
-- A `prepared` call may fail without ever going in flight, so `in_flight_at`
-- is required only of a call that actually reached that state.
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

--
-- The application row, written at apply time.
--

ALTER TABLE context_compaction
    ADD COLUMN applied_at timestamp with time zone
        DEFAULT statement_timestamp() NOT NULL;

--
-- Stamps do not move.
--
-- The four mutable lifecycle state machines police their transitions column by
-- column, so without these an otherwise valid transition could also rewrite a
-- stamp and silently falsify every duration, queue wait, and funnel interval
-- derived from it. The append-only families need nothing: `outbox_event`,
-- `delegation_outbox_event`, `session`, `goal_event`, and `context_compaction`
-- already reject every update.
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

--
-- The compaction command's result changes exactly once; its acceptance time
-- never does.
--

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
-- Each compaction transition stamp is written once, by the transition it
-- records. The state constraint above cannot say this on its own: an
-- `in_flight -> terminal` update satisfies it while clearing `in_flight_at`,
-- which would erase the interval the funnel measures.
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
