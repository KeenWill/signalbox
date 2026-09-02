--
-- Session lifecycle §5: per-consumer delivery cursors and the durable session
-- timeline projection they gate.
--
-- The singleton `outbox_delivery_state` becomes an authoritative registry with
-- one cursor per named consumer. `process_protocol` is the wire fan-out's,
-- carrying the singleton's exact position and discipline over both header
-- families. `session_timeline` is the timeline projection's, advanced inside
-- the appending transaction and never past an unprojected sequence.
--

SET check_function_bodies = false;

--
-- The timeline projection. Retention prunes headers and typed records; this
-- index of what happened is not pruned, so it carries no header foreign key.
--

CREATE TABLE session_timeline_item (
    event_sequence numeric(20,0) NOT NULL,
    session_id uuid,
    event_kind text NOT NULL,
    turn_disposition text,
    CONSTRAINT session_timeline_item_pkey PRIMARY KEY (event_sequence),
    CONSTRAINT session_timeline_item_sequence_positive_u64 CHECK (
        (event_sequence >= (1)::numeric)
        AND (event_sequence <= '18446744073709551615'::numeric)
    ),
    CONSTRAINT session_timeline_item_session_fk
        FOREIGN KEY (session_id) REFERENCES session (session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED
);

CREATE INDEX session_timeline_item_by_session_sequence
    ON session_timeline_item USING btree (session_id, event_sequence);

CREATE TRIGGER session_timeline_item_cannot_be_truncated
    BEFORE TRUNCATE ON session_timeline_item
    FOR EACH STATEMENT EXECUTE FUNCTION reject_outbox_table_truncate();

CREATE TRIGGER session_timeline_item_is_append_only
    BEFORE DELETE OR UPDATE ON session_timeline_item
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

INSERT INTO session_timeline_item (
    event_sequence, session_id, event_kind, turn_disposition
)
SELECT event_sequence, session_id, event_kind, turn_disposition
  FROM outbox_event
 UNION ALL
SELECT event_sequence, session_id, event_kind, NULL::text
  FROM delegation_outbox_event;

--
-- The consumer registry.
--

CREATE TABLE outbox_consumer_cursor (
    consumer_name text NOT NULL COLLATE pg_catalog."C",
    delivered_through numeric(20,0) NOT NULL,
    updated_at timestamp with time zone NOT NULL,
    last_delivery_xid xid8,
    CONSTRAINT outbox_consumer_cursor_pkey PRIMARY KEY (consumer_name),
    CONSTRAINT outbox_consumer_cursor_registered CHECK (
        consumer_name = ANY (ARRAY['process_protocol'::text, 'session_timeline'::text])
    ),
    CONSTRAINT outbox_consumer_cursor_u64 CHECK (
        (delivered_through >= (0)::numeric)
        AND (delivered_through <= '18446744073709551615'::numeric)
    ),
    CONSTRAINT outbox_consumer_cursor_transaction_recorded CHECK (
        (consumer_name <> 'process_protocol'::text)
        OR (delivered_through = (0)::numeric)
        OR (last_delivery_xid IS NOT NULL)
    )
);

INSERT INTO outbox_consumer_cursor (
    consumer_name, delivered_through, updated_at, last_delivery_xid
)
SELECT 'process_protocol', delivered_through, now(), last_delivery_xid
  FROM outbox_delivery_state
 WHERE singleton;

INSERT INTO outbox_consumer_cursor (
    consumer_name, delivered_through, updated_at, last_delivery_xid
)
SELECT 'session_timeline', last_sequence, now(), NULL
  FROM outbox_sequence_state
 WHERE singleton;

CREATE FUNCTION require_next_outbox_consumer_advance() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NEW.consumer_name <> OLD.consumer_name
        OR NEW.delivered_through <> OLD.delivered_through + 1 THEN
        RAISE EXCEPTION 'outbox delivery must advance by exactly one sequence'
            USING ERRCODE = '23514';
    END IF;
    IF NEW.consumer_name = 'process_protocol' THEN
        IF EXISTS (
            SELECT 1 FROM outbox_sequence_state
             WHERE singleton AND last_allocation_xid = pg_current_xact_id()
        ) THEN
            RAISE EXCEPTION
                'outbox delivery cannot advance in an event-producing transaction'
                USING ERRCODE = '23514';
        END IF;
        NEW.last_delivery_xid := pg_current_xact_id();
        IF NOT EXISTS (
            SELECT 1 FROM outbox_event
             WHERE event_sequence = NEW.delivered_through
        ) AND NOT EXISTS (
            SELECT 1 FROM delegation_outbox_event
             WHERE event_sequence = NEW.delivered_through
        ) THEN
            RAISE EXCEPTION 'outbox delivery sequence % requires a committed event',
                NEW.delivered_through
                USING ERRCODE = '23503';
        END IF;
    ELSE
        IF NOT EXISTS (
            SELECT 1 FROM session_timeline_item
             WHERE event_sequence = NEW.delivered_through
        ) THEN
            RAISE EXCEPTION
                'consumer % cannot advance past unprojected sequence %',
                NEW.consumer_name, NEW.delivered_through
                USING ERRCODE = '23503';
        END IF;
    END IF;
    NEW.updated_at := now();
    RETURN NEW;
END;
$$;

CREATE TRIGGER outbox_consumer_cursor_advances_prefix
    BEFORE UPDATE ON outbox_consumer_cursor
    FOR EACH ROW EXECUTE FUNCTION require_next_outbox_consumer_advance();

CREATE TRIGGER outbox_consumer_cursor_cannot_be_deleted
    BEFORE DELETE ON outbox_consumer_cursor
    FOR EACH ROW EXECUTE FUNCTION reject_outbox_state_delete();

CREATE TRIGGER outbox_consumer_cursor_cannot_be_truncated
    BEFORE TRUNCATE ON outbox_consumer_cursor
    FOR EACH STATEMENT EXECUTE FUNCTION reject_outbox_table_truncate();

--
-- Appending projects the header and advances the projection's own cursor in
-- the same transaction, so the cursor is never ahead of the projection.
--

CREATE FUNCTION project_session_timeline_item() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    INSERT INTO session_timeline_item (
        event_sequence, session_id, event_kind, turn_disposition
    )
    VALUES (
        NEW.event_sequence, NEW.session_id, NEW.event_kind,
        to_jsonb(NEW) ->> 'turn_disposition'
    );
    UPDATE outbox_consumer_cursor
       SET delivered_through = NEW.event_sequence
     WHERE consumer_name = 'session_timeline';
    RETURN NULL;
END;
$$;

CREATE TRIGGER outbox_event_projects_timeline_item
    AFTER INSERT ON outbox_event
    FOR EACH ROW EXECUTE FUNCTION project_session_timeline_item();

CREATE TRIGGER delegation_outbox_event_projects_timeline_item
    AFTER INSERT ON delegation_outbox_event
    FOR EACH ROW EXECUTE FUNCTION project_session_timeline_item();

--
-- The singleton is replaced, not kept beside its registry.
--

CREATE OR REPLACE FUNCTION allocate_outbox_event_sequence() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NEW.event_sequence IS NOT NULL THEN
        RAISE EXCEPTION 'outbox event sequence is allocator-owned'
            USING ERRCODE = '23514';
    END IF;

    IF EXISTS (
        SELECT 1
          FROM outbox_consumer_cursor
         WHERE consumer_name = 'process_protocol'
           AND last_delivery_xid = pg_current_xact_id()
    ) THEN
        RAISE EXCEPTION
            'outbox event append cannot follow delivery in one transaction'
            USING ERRCODE = '23514';
    END IF;

    UPDATE outbox_sequence_state
       SET last_sequence = last_sequence + 1,
           last_allocation_xid = pg_current_xact_id()
     WHERE singleton
       AND last_sequence < 18446744073709551615
    RETURNING last_sequence INTO NEW.event_sequence;

    IF NOT FOUND THEN
        RAISE EXCEPTION 'outbox event sequence exhausted'
            USING ERRCODE = '22003';
    END IF;

    RETURN NEW;
END;
$$;

DROP TABLE outbox_delivery_state;
DROP FUNCTION require_next_outbox_delivery();
