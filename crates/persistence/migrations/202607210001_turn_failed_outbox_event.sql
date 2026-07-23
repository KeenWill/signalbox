-- Typed outbox projection for the startup Lost recovery slice
-- (docs/spec/persistence-protocol.md).

ALTER TABLE outbox_event
    DROP CONSTRAINT outbox_event_kind_closed,
    DROP CONSTRAINT outbox_event_storage_version_supported;

ALTER TABLE outbox_event
    ADD CONSTRAINT outbox_event_kind_closed
        CHECK (event_kind IN ('session_created', 'turn_failed')),
    ADD CONSTRAINT outbox_event_storage_version_supported
        CHECK (
            (event_kind = 'session_created' AND storage_version = 1)
            OR (event_kind = 'turn_failed' AND storage_version = 1)
        );

CREATE TABLE turn_failed_outbox_event (
    event_sequence numeric(20, 0) PRIMARY KEY,
    event_kind text NOT NULL,
    storage_version smallint NOT NULL,
    session_id uuid NOT NULL,
    turn_id uuid NOT NULL UNIQUE,
    failure_entry_id uuid NOT NULL UNIQUE,
    terminal_frontier_id uuid NOT NULL UNIQUE,

    CONSTRAINT turn_failed_outbox_event_kind_closed
        CHECK (event_kind = 'turn_failed'),
    CONSTRAINT turn_failed_outbox_event_storage_version_supported
        CHECK (storage_version = 1),
    CONSTRAINT turn_failed_outbox_event_header_fk
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
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT turn_failed_outbox_event_turn_fk
        FOREIGN KEY (turn_id, session_id)
        REFERENCES turn_lifecycle (turn_id, session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT turn_failed_outbox_event_failure_entry_fk
        FOREIGN KEY (session_id, failure_entry_id)
        REFERENCES semantic_transcript_entry (
            source_session_id,
            semantic_entry_id
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT turn_failed_outbox_event_terminal_frontier_fk
        FOREIGN KEY (session_id, terminal_frontier_id)
        REFERENCES context_frontier (owning_session_id, context_frontier_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE OR REPLACE FUNCTION require_outbox_event_typed_record()
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
        WHEN 'turn_failed' THEN
            SELECT count(*)
              INTO matching_records
              FROM turn_failed_outbox_event
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

CREATE TRIGGER turn_failed_outbox_event_is_append_only
BEFORE UPDATE OR DELETE ON turn_failed_outbox_event
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER turn_failed_outbox_event_cannot_be_truncated
BEFORE TRUNCATE ON turn_failed_outbox_event
FOR EACH STATEMENT
EXECUTE FUNCTION reject_outbox_table_truncate();
