--
-- Session lifecycle §5: floor-bounded deletion on both outbox header families.
--
-- Deletion is legal only at or below the retention floor, which is
-- `min(delivered_through)` over the consumer registry. Typed records are
-- deleted before their header, which is what their `ON DELETE RESTRICT` header
-- foreign keys admit. `TRUNCATE` stays rejected everywhere: truncation is
-- whole-table and cannot respect a floor.
--

SET check_function_bodies = false;

CREATE TABLE outbox_retention_state (
    singleton boolean NOT NULL,
    pruned_through numeric(20,0) NOT NULL,
    updated_at timestamp with time zone NOT NULL,
    CONSTRAINT outbox_retention_state_pkey PRIMARY KEY (singleton),
    CONSTRAINT outbox_retention_state_singleton CHECK (singleton),
    CONSTRAINT outbox_retention_state_u64 CHECK (
        (pruned_through >= (0)::numeric)
        AND (pruned_through <= '18446744073709551615'::numeric)
    )
);

INSERT INTO outbox_retention_state (singleton, pruned_through, updated_at)
VALUES (true, 0, now());

CREATE FUNCTION outbox_retention_floor() RETURNS numeric
    LANGUAGE sql STABLE
    AS $$
    SELECT coalesce(min(delivered_through), (0)::numeric)
      FROM outbox_consumer_cursor;
$$;

CREATE FUNCTION reject_outbox_record_change() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF TG_OP <> 'DELETE' THEN
        RAISE EXCEPTION '% is append-only', TG_TABLE_NAME
            USING ERRCODE = '23514';
    END IF;
    IF OLD.event_sequence > outbox_retention_floor() THEN
        RAISE EXCEPTION
            '% sequence % is above the outbox retention floor',
            TG_TABLE_NAME, OLD.event_sequence
            USING ERRCODE = '23514';
    END IF;
    RETURN OLD;
END;
$$;

CREATE FUNCTION require_next_outbox_prune() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NEW.singleton <> OLD.singleton
        OR NEW.pruned_through < OLD.pruned_through THEN
        RAISE EXCEPTION 'outbox pruning cannot retreat'
            USING ERRCODE = '23514';
    END IF;
    IF NEW.pruned_through > outbox_retention_floor() THEN
        RAISE EXCEPTION
            'outbox pruning cannot pass the retention floor'
            USING ERRCODE = '23514';
    END IF;
    NEW.updated_at := now();
    RETURN NEW;
END;
$$;

CREATE TRIGGER outbox_retention_state_advances_prune
    BEFORE UPDATE ON outbox_retention_state
    FOR EACH ROW EXECUTE FUNCTION require_next_outbox_prune();

CREATE TRIGGER outbox_retention_state_cannot_be_deleted
    BEFORE DELETE ON outbox_retention_state
    FOR EACH ROW EXECUTE FUNCTION reject_outbox_state_delete();

CREATE TRIGGER outbox_retention_state_cannot_be_truncated
    BEFORE TRUNCATE ON outbox_retention_state
    FOR EACH STATEMENT EXECUTE FUNCTION reject_outbox_table_truncate();

--
-- Every outbox table swaps its plain append-only guard for the floor-bounded
-- one. The block first proves the list names every typed table that
-- references either header, so a table added later cannot slip past it.
--

DO $$
DECLARE
    guarded text[] := ARRAY[
        'outbox_event',
        'session_created_outbox_event',
        'session_model_settings_changed_outbox_event',
        'turn_model_settings_resolved_outbox_event',
        'input_accepted_outbox_event',
        'turn_activated_outbox_event',
        'turn_terminal_outbox_event',
        'model_call_transition_outbox_event',
        'tool_batch_transition_outbox_event',
        'tool_approval_decided_outbox_event',
        'context_compacted_outbox_event',
        'runner_state_transition_outbox_event',
        'session_state_changed_outbox_event',
        'session_terminal_outbox_event',
        'goal_changed_outbox_event',
        'session_ownership_changed_outbox_event',
        'command_settled_outbox_event',
        'injection_settled_outbox_event',
        'delegation_outbox_event',
        'delegation_update_outbox_event',
        'delegation_wake_outbox_event'
    ];
    unlisted text[];
    guarded_table text;
BEGIN
    SELECT array_agg(DISTINCT child.relname::text)
      INTO unlisted
      FROM pg_constraint AS reference
      JOIN pg_class AS child ON child.oid = reference.conrelid
      JOIN pg_class AS parent ON parent.oid = reference.confrelid
     WHERE reference.contype = 'f'
       AND parent.relname IN ('outbox_event', 'delegation_outbox_event')
       AND NOT (child.relname::text = ANY (guarded));
    IF unlisted IS NOT NULL THEN
        RAISE EXCEPTION
            'outbox typed tables missing the retention guard: %', unlisted;
    END IF;
    FOREACH guarded_table IN ARRAY guarded LOOP
        -- Supersedes 202609010000_core, 202609010001_sessions,
        -- 202609010002_turns, 202609010003_model_calls,
        -- 202609010004_tool_loop, 202609010005_delegation,
        -- 202609010008_runners, and 202609020004_event_vocabulary.
        EXECUTE format(
            'DROP TRIGGER %I ON %I',
            guarded_table || '_is_append_only', guarded_table
        );
        EXECUTE format(
            'CREATE TRIGGER %I BEFORE DELETE OR UPDATE ON %I'
            ' FOR EACH ROW EXECUTE FUNCTION reject_outbox_record_change()',
            guarded_table || '_is_append_only', guarded_table
        );
    END LOOP;
END;
$$;

RESET check_function_bodies;
