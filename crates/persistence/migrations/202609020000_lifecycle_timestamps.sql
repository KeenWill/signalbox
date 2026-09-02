--
-- Session lifecycle §3: write-time stamps on the lifecycle rows.
--
-- Every column lands NOT NULL outright. The unconstrained dogfood-database
-- reset is ratified, so no row predates these columns and no backfill marker
-- or `CHECK ... NOT VALID` scaffolding is owed. `clock_timestamp()` is the
-- statement clock, so a stamp records when its own row was written rather than
-- when the enclosing transaction opened; the repository writes never restate
-- it, which is what makes the stamp impossible for a new write path to skip.
--

--
-- Outbox headers. Both families carry the event's own record time.
--

ALTER TABLE outbox_event
    ADD COLUMN recorded_at timestamp with time zone
        DEFAULT clock_timestamp() NOT NULL;

ALTER TABLE delegation_outbox_event
    ADD COLUMN recorded_at timestamp with time zone
        DEFAULT clock_timestamp() NOT NULL;

--
-- Session creation. `session` is append-only, so the row's insert time is its
-- creation time. `ended_at` is the lifecycle satellite's, not this table's.
--

ALTER TABLE session
    ADD COLUMN created_at timestamp with time zone
        DEFAULT clock_timestamp() NOT NULL;

--
-- The five lifecycle rows §3 names. Each is stamped when the row is written;
-- the mutable state machines (`turn_lifecycle`, `turn_attempt`, `model_call`,
-- `tool_attempt`) keep their transition history in their own state columns, so
-- this records the row's origin instant and never moves.
--

ALTER TABLE turn_lifecycle
    ADD COLUMN recorded_at timestamp with time zone
        DEFAULT clock_timestamp() NOT NULL;

ALTER TABLE turn_attempt
    ADD COLUMN recorded_at timestamp with time zone
        DEFAULT clock_timestamp() NOT NULL;

ALTER TABLE model_call
    ADD COLUMN recorded_at timestamp with time zone
        DEFAULT clock_timestamp() NOT NULL;

ALTER TABLE tool_attempt
    ADD COLUMN recorded_at timestamp with time zone
        DEFAULT clock_timestamp() NOT NULL;

ALTER TABLE goal_event
    ADD COLUMN recorded_at timestamp with time zone
        DEFAULT clock_timestamp() NOT NULL;

--
-- Compaction lifecycle. §3 requires the command's acceptance to carry its own
-- durable `requested_at`: `durable_command.claimed_at` is non-semantic
-- operational metadata and never stands in for it.
--

ALTER TABLE compact_session_command
    ADD COLUMN requested_at timestamp with time zone
        DEFAULT clock_timestamp() NOT NULL;

--
-- The compaction call's three transitions. `prepared_at` is the insert stamp;
-- the repository sets the other two in the same statement that moves
-- `state_kind`, so a stamp cannot disagree with the state it records.
-- A `prepared` call may fail without ever going in flight, so `in_flight_at`
-- is required only of a call that actually reached that state.
--

ALTER TABLE context_compaction_model_call
    ADD COLUMN prepared_at timestamp with time zone
        DEFAULT clock_timestamp() NOT NULL,
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
        DEFAULT clock_timestamp() NOT NULL;
