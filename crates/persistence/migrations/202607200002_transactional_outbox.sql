-- ADR-0040 transactional-outbox storage foundation.
--
-- A transaction reserves an event sequence by inserting an outbox header.
-- The allocation trigger updates the singleton sequence row, whose row lock is
-- retained until transaction end. A later allocator therefore cannot obtain a
-- higher sequence until every lower allocation has committed or rolled back.
-- The update and event insert roll back together, so committed sequences are
-- contiguous as well as commit ordered.

CREATE TABLE outbox_sequence_state (
    singleton boolean PRIMARY KEY,
    last_sequence numeric(20, 0) NOT NULL,

    CONSTRAINT outbox_sequence_state_singleton
        CHECK (singleton),
    CONSTRAINT outbox_sequence_state_u64
        CHECK (
            last_sequence >= 0
            AND last_sequence <= 18446744073709551615
        )
);

INSERT INTO outbox_sequence_state (singleton, last_sequence)
VALUES (TRUE, 0);

CREATE TABLE outbox_delivery_state (
    singleton boolean PRIMARY KEY,
    delivered_through numeric(20, 0) NOT NULL,

    CONSTRAINT outbox_delivery_state_singleton
        CHECK (singleton),
    CONSTRAINT outbox_delivery_state_u64
        CHECK (
            delivered_through >= 0
            AND delivered_through <= 18446744073709551615
        )
);

INSERT INTO outbox_delivery_state (singleton, delivered_through)
VALUES (TRUE, 0);

CREATE TABLE outbox_event (
    event_sequence numeric(20, 0) PRIMARY KEY,
    event_kind text NOT NULL,
    storage_version smallint NOT NULL,
    session_id uuid NOT NULL,

    CONSTRAINT outbox_event_sequence_positive_u64
        CHECK (
            event_sequence >= 1
            AND event_sequence <= 18446744073709551615
        ),
    CONSTRAINT outbox_event_kind_closed
        CHECK (event_kind = 'session_created'),
    CONSTRAINT outbox_event_storage_version_supported
        CHECK (storage_version = 1),
    CONSTRAINT outbox_event_typed_record_key
        UNIQUE (
            event_sequence,
            event_kind,
            storage_version,
            session_id
        ),
    CONSTRAINT outbox_event_session_fk
        FOREIGN KEY (session_id)
        REFERENCES session (session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE INDEX outbox_event_by_session_sequence
    ON outbox_event (session_id, event_sequence);

CREATE TABLE session_created_outbox_event (
    event_sequence numeric(20, 0) PRIMARY KEY,
    event_kind text NOT NULL,
    storage_version smallint NOT NULL,
    session_id uuid NOT NULL UNIQUE,

    CONSTRAINT session_created_outbox_event_kind_closed
        CHECK (event_kind = 'session_created'),
    CONSTRAINT session_created_outbox_event_storage_version_supported
        CHECK (storage_version = 1),
    CONSTRAINT session_created_outbox_event_header_fk
        FOREIGN KEY (
            event_sequence,
            event_kind,
            storage_version,
            session_id
        )
        REFERENCES outbox_event (
            event_sequence,
            event_kind,
            storage_version,
            session_id
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE FUNCTION allocate_outbox_event_sequence()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.event_sequence IS NOT NULL THEN
        RAISE EXCEPTION 'outbox event sequence is allocator-owned'
            USING ERRCODE = '23514';
    END IF;

    UPDATE outbox_sequence_state
       SET last_sequence = last_sequence + 1
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

CREATE TRIGGER outbox_event_allocates_sequence
BEFORE INSERT ON outbox_event
FOR EACH ROW
EXECUTE FUNCTION allocate_outbox_event_sequence();

CREATE FUNCTION require_outbox_sequence_event()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.last_sequence <> OLD.last_sequence + 1 THEN
        RAISE EXCEPTION 'outbox sequence must advance exactly once per event'
            USING ERRCODE = '23514';
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM outbox_event
         WHERE event_sequence = NEW.last_sequence
    ) THEN
        RAISE EXCEPTION
            'outbox sequence % requires its event row',
            NEW.last_sequence
            USING ERRCODE = '23503';
    END IF;

    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER outbox_sequence_requires_event
AFTER UPDATE ON outbox_sequence_state
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_outbox_sequence_event();

CREATE FUNCTION require_outbox_event_typed_record()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    matching_records bigint;
BEGIN
    CASE NEW.event_kind
        WHEN 'session_created' THEN
            SELECT count(*)
              INTO matching_records
              FROM session_created_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        ELSE
            RAISE EXCEPTION 'unsupported outbox event kind %', NEW.event_kind
                USING ERRCODE = '23514';
    END CASE;

    IF matching_records <> 1 THEN
        RAISE EXCEPTION
            'outbox event % requires exactly one % typed record',
            NEW.event_sequence,
            NEW.event_kind
            USING ERRCODE = '23503';
    END IF;

    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER outbox_event_requires_typed_record
AFTER INSERT ON outbox_event
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_outbox_event_typed_record();

CREATE FUNCTION require_next_outbox_delivery()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.singleton <> OLD.singleton
        OR NEW.delivered_through <> OLD.delivered_through + 1
    THEN
        RAISE EXCEPTION
            'outbox delivery must advance by exactly one sequence'
            USING ERRCODE = '23514';
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM outbox_event
         WHERE event_sequence = NEW.delivered_through
    ) THEN
        RAISE EXCEPTION
            'outbox delivery sequence % requires a committed event',
            NEW.delivered_through
            USING ERRCODE = '23503';
    END IF;

    RETURN NEW;
END;
$$;

CREATE TRIGGER outbox_delivery_advances_prefix
BEFORE UPDATE ON outbox_delivery_state
FOR EACH ROW
EXECUTE FUNCTION require_next_outbox_delivery();

CREATE FUNCTION reject_outbox_state_delete()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION '% singleton cannot be deleted', TG_TABLE_NAME
        USING ERRCODE = '23514';
END;
$$;

CREATE FUNCTION reject_outbox_table_truncate()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION '% cannot be truncated', TG_TABLE_NAME
        USING ERRCODE = '23514';
END;
$$;

CREATE TRIGGER outbox_sequence_state_cannot_be_deleted
BEFORE DELETE ON outbox_sequence_state
FOR EACH ROW
EXECUTE FUNCTION reject_outbox_state_delete();

CREATE TRIGGER outbox_delivery_state_cannot_be_deleted
BEFORE DELETE ON outbox_delivery_state
FOR EACH ROW
EXECUTE FUNCTION reject_outbox_state_delete();

CREATE TRIGGER outbox_event_is_append_only
BEFORE UPDATE OR DELETE ON outbox_event
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER session_created_outbox_event_is_append_only
BEFORE UPDATE OR DELETE ON session_created_outbox_event
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER outbox_sequence_state_cannot_be_truncated
BEFORE TRUNCATE ON outbox_sequence_state
FOR EACH STATEMENT
EXECUTE FUNCTION reject_outbox_table_truncate();

CREATE TRIGGER outbox_delivery_state_cannot_be_truncated
BEFORE TRUNCATE ON outbox_delivery_state
FOR EACH STATEMENT
EXECUTE FUNCTION reject_outbox_table_truncate();

CREATE TRIGGER outbox_event_cannot_be_truncated
BEFORE TRUNCATE ON outbox_event
FOR EACH STATEMENT
EXECUTE FUNCTION reject_outbox_table_truncate();

CREATE TRIGGER session_created_outbox_event_cannot_be_truncated
BEFORE TRUNCATE ON session_created_outbox_event
FOR EACH STATEMENT
EXECUTE FUNCTION reject_outbox_table_truncate();
