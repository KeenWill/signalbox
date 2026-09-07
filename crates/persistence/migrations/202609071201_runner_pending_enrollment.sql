-- Pending successor authority is activated only by a recovery command.

ALTER TABLE runner_enrollment DROP CONSTRAINT runner_enrollment_state_shape;
ALTER TABLE runner_enrollment ADD CONSTRAINT runner_enrollment_state_shape CHECK (
    (revision = 1 AND state_kind IN ('pending', 'active'))
    OR (revision = 2 AND state_kind IN ('active', 'revoked'))
    OR (revision = 3 AND state_kind = 'revoked')
);
ALTER TABLE runner_enrollment_audit DROP CONSTRAINT runner_enrollment_audit_state_shape;
ALTER TABLE runner_enrollment_audit DROP CONSTRAINT runner_enrollment_audit_state_closed;
ALTER TABLE runner_enrollment_audit ADD CONSTRAINT runner_enrollment_audit_state_shape CHECK (
    (revision = 1 AND state_kind IN ('pending', 'active'))
    OR (revision = 2 AND state_kind IN ('active', 'revoked'))
    OR (revision = 3 AND state_kind = 'revoked')
);

CREATE UNIQUE INDEX runner_one_pending_enrollment
    ON runner_enrollment ((state_kind)) WHERE state_kind = 'pending';

CREATE TABLE runner_pending_predecessor (
    enrollment_id uuid PRIMARY KEY REFERENCES runner_enrollment(enrollment_id),
    predecessor_enrollment_id uuid NOT NULL REFERENCES runner_enrollment(enrollment_id),
    CHECK (enrollment_id <> predecessor_enrollment_id)
);
CREATE TRIGGER runner_pending_predecessor_is_append_only
    BEFORE UPDATE OR DELETE ON runner_pending_predecessor
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE OR REPLACE FUNCTION guard_runner_registration_insert() RETURNS trigger
    LANGUAGE plpgsql AS $$
DECLARE
    enrollment_state text;
    latest_revision numeric;
BEGIN
    SELECT state_kind INTO enrollment_state FROM runner_enrollment
        WHERE enrollment_id = NEW.enrollment_id FOR SHARE;
    SELECT max(registration_revision) INTO latest_revision FROM runner_registration
        WHERE enrollment_id = NEW.enrollment_id;
    IF (enrollment_state <> 'active'
        AND NOT (enrollment_state = 'pending' AND latest_revision IS NULL))
        OR NEW.registration_revision <> COALESCE(latest_revision + 1, 1)
    THEN
        RAISE EXCEPTION 'runner registration lacks initial pending or active successor authority'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE OR REPLACE FUNCTION guard_runner_enrollment_change() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        IF NEW.revision <> 1 OR NEW.state_kind NOT IN ('pending', 'active') THEN
            RAISE EXCEPTION 'runner enrollment must begin pending or active at revision one'
                USING ERRCODE = '23514';
        END IF;
        RETURN NEW;
    END IF;
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'runner enrollment is not deletable'
            USING ERRCODE = '23514';
    END IF;
    IF ROW(
        OLD.enrollment_id,
        OLD.runner_id,
        OLD.authentication_reference_id,
        OLD.allowed_class_count
    ) IS DISTINCT FROM ROW(
        NEW.enrollment_id,
        NEW.runner_id,
        NEW.authentication_reference_id,
        NEW.allowed_class_count
    )
       OR NEW.revision <> OLD.revision + 1
       OR NOT (
           (OLD.state_kind = 'pending' AND NEW.state_kind = 'active')
           OR (OLD.state_kind = 'active' AND NEW.state_kind = 'revoked')
       )
    THEN
        RAISE EXCEPTION 'runner enrollment transition is not promotion or revocation'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE OR REPLACE FUNCTION guard_runner_connection_event_insert() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    prior runner_connection_event%ROWTYPE;
BEGIN
    PERFORM 1
      FROM runner_enrollment
     WHERE enrollment_id = NEW.enrollment_id
       AND state_kind IN ('active', 'pending')
       FOR UPDATE;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'runner connection requires active or pending enrollment'
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
