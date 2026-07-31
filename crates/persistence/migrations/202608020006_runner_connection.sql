-- Durable runner enrollment request receipts.

CREATE TABLE runner_enrollment_request_receipt (
    request_id uuid PRIMARY KEY,
    enrollment_id uuid NOT NULL UNIQUE,
    runner_id uuid NOT NULL UNIQUE,
    authentication_reference_id uuid NOT NULL UNIQUE,
    registration_revision numeric(20, 0) NOT NULL,

    CONSTRAINT runner_enrollment_request_receipt_initial_revision
        CHECK (registration_revision = 1),
    CONSTRAINT runner_enrollment_request_receipt_registration_fk
        FOREIGN KEY (
            enrollment_id,
            registration_revision,
            runner_id,
            authentication_reference_id
        )
        REFERENCES runner_registration (
            enrollment_id,
            registration_revision,
            runner_id,
            authentication_reference_id
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE TRIGGER runner_enrollment_request_receipt_is_append_only
BEFORE UPDATE OR DELETE ON runner_enrollment_request_receipt
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER runner_enrollment_request_receipt_rejects_truncate
BEFORE TRUNCATE ON runner_enrollment_request_receipt
FOR EACH STATEMENT
EXECUTE FUNCTION reject_immutable_record_change();

-- One append-only lifecycle stream per enrollment. A new connection advances
-- the epoch; heartbeat recovery advances only the event ordinal within it.
CREATE TABLE runner_connection_event (
    enrollment_id uuid NOT NULL,
    connection_epoch numeric(20, 0) NOT NULL,
    event_ordinal numeric(20, 0) NOT NULL,
    state_kind text NOT NULL,
    cause_kind text NOT NULL,
    recorded_at timestamptz NOT NULL DEFAULT transaction_timestamp(),

    CONSTRAINT runner_connection_event_pk
        PRIMARY KEY (enrollment_id, connection_epoch, event_ordinal),
    CONSTRAINT runner_connection_event_enrollment_fk
        FOREIGN KEY (enrollment_id)
        REFERENCES runner_enrollment (enrollment_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CONSTRAINT runner_connection_event_positive_u64
        CHECK (
            connection_epoch BETWEEN 1 AND 18446744073709551615
            AND event_ordinal BETWEEN 1 AND 18446744073709551615
        ),
    CONSTRAINT runner_connection_event_state_closed
        CHECK (state_kind IN ('connected', 'suspect', 'shutdown', 'lost')),
    CONSTRAINT runner_connection_event_cause_shape
        CHECK (
            (state_kind = 'connected'
                AND cause_kind IN ('established', 'heartbeat_recovered'))
            OR (state_kind = 'suspect' AND cause_kind = 'heartbeat_missed')
            OR (state_kind = 'shutdown'
                AND cause_kind IN ('daemon_shutdown', 'runner_shutdown'))
            OR (state_kind = 'lost'
                AND cause_kind IN (
                    'heartbeat_timeout',
                    'transport_closed',
                    'protocol_failure',
                    'enrollment_revoked'
                ))
        )
);

CREATE FUNCTION guard_runner_connection_event_insert()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    prior runner_connection_event%ROWTYPE;
BEGIN
    PERFORM 1
      FROM runner_enrollment
     WHERE enrollment_id = NEW.enrollment_id
       AND state_kind = 'active'
       FOR UPDATE;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'runner connection requires active enrollment'
            USING ERRCODE = '23514';
    END IF;

    SELECT *
      INTO prior
      FROM runner_connection_event
     WHERE enrollment_id = NEW.enrollment_id
     ORDER BY connection_epoch DESC, event_ordinal DESC
     LIMIT 1;

    IF NOT FOUND THEN
        IF NEW.connection_epoch <> 1
            OR NEW.event_ordinal <> 1
            OR NEW.state_kind <> 'connected'
            OR NEW.cause_kind <> 'established'
        THEN
            RAISE EXCEPTION 'invalid initial runner connection event'
                USING ERRCODE = '23514';
        END IF;
        RETURN NEW;
    END IF;

    IF NEW.connection_epoch = prior.connection_epoch + 1 THEN
        IF NEW.event_ordinal <> 1
            OR NEW.state_kind <> 'connected'
            OR NEW.cause_kind <> 'established'
        THEN
            RAISE EXCEPTION 'invalid successor runner connection event'
                USING ERRCODE = '23514';
        END IF;
        RETURN NEW;
    END IF;

    IF NEW.connection_epoch <> prior.connection_epoch
        OR NEW.event_ordinal <> prior.event_ordinal + 1
        OR prior.state_kind IN ('shutdown', 'lost')
        OR (prior.state_kind = 'connected' AND NEW.state_kind NOT IN ('suspect', 'shutdown', 'lost'))
        OR (prior.state_kind = 'suspect' AND NEW.state_kind NOT IN ('connected', 'shutdown', 'lost'))
    THEN
        RAISE EXCEPTION 'invalid runner connection transition'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER runner_connection_event_insert_is_guarded
BEFORE INSERT ON runner_connection_event
FOR EACH ROW
EXECUTE FUNCTION guard_runner_connection_event_insert();

CREATE TRIGGER runner_connection_event_is_append_only
BEFORE UPDATE OR DELETE ON runner_connection_event
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER runner_connection_event_rejects_truncate
BEFORE TRUNCATE ON runner_connection_event
FOR EACH STATEMENT
EXECUTE FUNCTION reject_immutable_record_change();
