-- Durable process-runtime fencing and the remaining client-visible scheduling
-- transitions admitted by docs/spec/process-protocol.md.

CREATE TABLE hub_fence_state (
    singleton boolean PRIMARY KEY DEFAULT TRUE,
    generation numeric(20, 0) NOT NULL,

    CONSTRAINT hub_fence_state_singleton
        CHECK (singleton),
    CONSTRAINT hub_fence_state_generation_positive_u64
        CHECK (
            generation >= 1
            AND generation <= 18446744073709551615
        )
);

INSERT INTO hub_fence_state (singleton, generation)
VALUES (TRUE, 1);

CREATE FUNCTION reject_invalid_hub_fence_change()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'hub fence state cannot be deleted'
            USING ERRCODE = '23514';
    END IF;

    IF NEW.singleton IS DISTINCT FROM OLD.singleton
       OR NEW.generation IS DISTINCT FROM OLD.generation + 1 THEN
        RAISE EXCEPTION 'hub fence generation must advance exactly once'
            USING ERRCODE = '23514';
    END IF;

    RETURN NEW;
END;
$$;

CREATE TRIGGER hub_fence_state_change_is_guarded
BEFORE UPDATE OR DELETE ON hub_fence_state
FOR EACH ROW
EXECUTE FUNCTION reject_invalid_hub_fence_change();

CREATE TRIGGER hub_fence_state_cannot_be_truncated
BEFORE TRUNCATE ON hub_fence_state
FOR EACH STATEMENT
EXECUTE FUNCTION reject_outbox_table_truncate();

ALTER TABLE outbox_event
    DROP CONSTRAINT outbox_event_kind_closed;

ALTER TABLE outbox_event
    ADD CONSTRAINT outbox_event_kind_closed
        CHECK (
            event_kind IN (
                'session_created',
                'input_accepted',
                'turn_activated',
                'turn_failed',
                'model_call_transition',
                'turn_completed',
                'turn_refused',
                'turn_cancelled',
                'turn_reconciliation_required'
            )
        );

CREATE TABLE input_accepted_outbox_event (
    event_sequence numeric(20, 0) PRIMARY KEY,
    event_kind text NOT NULL,
    storage_version smallint NOT NULL,
    session_id uuid NOT NULL,
    accepted_input_id uuid NOT NULL UNIQUE,
    turn_id uuid NOT NULL UNIQUE,
    acceptance_position numeric(20, 0) NOT NULL,

    CONSTRAINT input_accepted_outbox_kind_closed
        CHECK (event_kind = 'input_accepted'),
    CONSTRAINT input_accepted_outbox_version_supported
        CHECK (storage_version = 1),
    CONSTRAINT input_accepted_outbox_header_fk
        FOREIGN KEY (event_sequence, event_kind, storage_version, session_id)
        REFERENCES outbox_event (
            event_sequence,
            event_kind,
            storage_version,
            session_id
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT input_accepted_outbox_origin_fk
        FOREIGN KEY (
            accepted_input_id,
            session_id,
            acceptance_position,
            turn_id
        )
        REFERENCES accepted_input (
            accepted_input_id,
            session_id,
            acceptance_position,
            origin_turn_id
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE TABLE turn_activated_outbox_event (
    event_sequence numeric(20, 0) PRIMARY KEY,
    event_kind text NOT NULL,
    storage_version smallint NOT NULL,
    session_id uuid NOT NULL,
    turn_id uuid NOT NULL UNIQUE,
    current_attempt_id uuid NOT NULL UNIQUE,

    CONSTRAINT turn_activated_outbox_kind_closed
        CHECK (event_kind = 'turn_activated'),
    CONSTRAINT turn_activated_outbox_version_supported
        CHECK (storage_version = 1),
    CONSTRAINT turn_activated_outbox_header_fk
        FOREIGN KEY (event_sequence, event_kind, storage_version, session_id)
        REFERENCES outbox_event (
            event_sequence,
            event_kind,
            storage_version,
            session_id
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT turn_activated_outbox_attempt_fk
        FOREIGN KEY (current_attempt_id, turn_id, session_id)
        REFERENCES turn_attempt (turn_attempt_id, turn_id, session_id)
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
            SELECT count(*) INTO matching_records
              FROM session_created_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'input_accepted' THEN
            SELECT count(*) INTO matching_records
              FROM input_accepted_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'turn_activated' THEN
            SELECT count(*) INTO matching_records
              FROM turn_activated_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'turn_failed' THEN
            SELECT count(*) INTO matching_records
              FROM turn_failed_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'model_call_transition' THEN
            SELECT count(*) INTO matching_records
              FROM model_call_transition_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'turn_completed' THEN
            SELECT count(*) INTO matching_records
              FROM turn_completed_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'turn_refused' THEN
            SELECT count(*) INTO matching_records
              FROM turn_refused_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'turn_cancelled' THEN
            SELECT count(*) INTO matching_records
              FROM turn_cancelled_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'turn_reconciliation_required' THEN
            SELECT count(*) INTO matching_records
              FROM turn_reconciliation_required_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        ELSE
            RAISE EXCEPTION 'unsupported outbox event kind %', NEW.event_kind
                USING ERRCODE = '23514';
    END CASE;

    IF matching_records <> 1 THEN
        RAISE EXCEPTION 'outbox event % requires exactly one % typed record',
            NEW.event_sequence,
            NEW.event_kind
            USING ERRCODE = '23503';
    END IF;
    RETURN NULL;
END;
$$;

CREATE TRIGGER input_accepted_outbox_event_is_append_only
BEFORE UPDATE OR DELETE ON input_accepted_outbox_event
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER turn_activated_outbox_event_is_append_only
BEFORE UPDATE OR DELETE ON turn_activated_outbox_event
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER input_accepted_outbox_event_cannot_be_truncated
BEFORE TRUNCATE ON input_accepted_outbox_event
FOR EACH STATEMENT
EXECUTE FUNCTION reject_outbox_table_truncate();

CREATE TRIGGER turn_activated_outbox_event_cannot_be_truncated
BEFORE TRUNCATE ON turn_activated_outbox_event
FOR EACH STATEMENT
EXECUTE FUNCTION reject_outbox_table_truncate();
