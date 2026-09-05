-- The process runtime and repository-watch module consume the same committed
-- outbox independently. Each advances only its own delivery prefix.

SET check_function_bodies = false;

CREATE TABLE outbox_consumer_cursor (
    consumer_name text NOT NULL,
    delivered_through numeric(20,0) NOT NULL,
    last_delivery_xid xid8,
    CONSTRAINT outbox_consumer_cursor_pkey PRIMARY KEY (consumer_name),
    CONSTRAINT outbox_delivery_consumer_closed CHECK (
        consumer_name = ANY (ARRAY[
            'process_protocol'::text,
            'repo_watch'::text
        ])
    ),
    CONSTRAINT outbox_consumer_cursor_transaction_recorded CHECK (
        delivered_through = 0 OR last_delivery_xid IS NOT NULL
    ),
    CONSTRAINT outbox_consumer_cursor_u64 CHECK (
        delivered_through >= 0
        AND delivered_through <= 18446744073709551615
    )
);

CREATE FUNCTION require_next_outbox_consumer_delivery() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NEW.consumer_name <> OLD.consumer_name
        OR NEW.delivered_through <> OLD.delivered_through + 1 THEN
        RAISE EXCEPTION 'outbox delivery must advance by exactly one sequence'
            USING ERRCODE = '23514';
    END IF;
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
            NEW.delivered_through USING ERRCODE = '23503';
    END IF;
    RETURN NEW;
END;
$$;

CREATE OR REPLACE FUNCTION allocate_outbox_event_sequence() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NEW.event_sequence IS NOT NULL THEN
        RAISE EXCEPTION 'outbox event sequence is allocator-owned'
            USING ERRCODE = '23514';
    END IF;

    IF EXISTS (
        SELECT 1 FROM outbox_consumer_cursor
         WHERE last_delivery_xid = pg_current_xact_id()
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

CREATE TRIGGER outbox_consumer_cursor_advances_prefix
    BEFORE UPDATE ON outbox_consumer_cursor
    FOR EACH ROW EXECUTE FUNCTION require_next_outbox_consumer_delivery();

CREATE TRIGGER outbox_consumer_cursor_cannot_be_deleted
    BEFORE DELETE ON outbox_consumer_cursor
    FOR EACH ROW EXECUTE FUNCTION reject_outbox_state_delete();

CREATE TRIGGER outbox_consumer_cursor_cannot_be_truncated
    BEFORE TRUNCATE ON outbox_consumer_cursor
    FOR EACH STATEMENT EXECUTE FUNCTION reject_outbox_table_truncate();

INSERT INTO outbox_consumer_cursor
    (consumer_name, delivered_through, last_delivery_xid)
SELECT 'process_protocol', delivered_through, last_delivery_xid
  FROM outbox_delivery_state
 WHERE singleton;

INSERT INTO outbox_consumer_cursor
    (consumer_name, delivered_through, last_delivery_xid)
VALUES ('repo_watch', 0, NULL);

DROP TABLE outbox_delivery_state;
DROP FUNCTION require_next_outbox_delivery();

RESET check_function_bodies;
