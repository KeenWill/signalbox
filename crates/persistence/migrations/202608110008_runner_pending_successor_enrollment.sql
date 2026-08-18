-- Admit one provisioning-only pending successor after exact durable runner loss.

ALTER TABLE runner_enrollment_request_receipt
    ADD COLUMN authority_kind text NOT NULL DEFAULT 'active';

ALTER TABLE runner_enrollment_request_receipt
    ALTER COLUMN authority_kind DROP DEFAULT,
    ADD CONSTRAINT runner_enrollment_request_receipt_authority_closed
        CHECK (authority_kind IN ('active', 'replacement_pending')),
    ADD CONSTRAINT runner_enrollment_request_receipt_request_enrollment_key
        UNIQUE (request_id, enrollment_id);

-- Supersedes the enrollment state constraints from
-- 202607280401_runner_protocol.sql. Promotion remains a later transaction, so
-- pending is currently an immutable revision-one state.
ALTER TABLE runner_enrollment_audit
    DROP CONSTRAINT runner_enrollment_audit_state_closed,
    DROP CONSTRAINT runner_enrollment_audit_state_shape,
    ADD CONSTRAINT runner_enrollment_audit_state_closed
        CHECK (state_kind IN ('pending', 'active', 'revoked')),
    ADD CONSTRAINT runner_enrollment_audit_state_shape
        CHECK (
            (revision = 1 AND state_kind IN ('pending', 'active'))
            OR (revision = 2 AND state_kind = 'revoked')
        );

ALTER TABLE runner_enrollment
    DROP CONSTRAINT runner_enrollment_state_shape,
    ADD CONSTRAINT runner_enrollment_state_shape
        CHECK (
            (revision = 1 AND state_kind IN ('pending', 'active'))
            OR (revision = 2 AND state_kind = 'revoked')
        );

-- Supersedes guard_runner_enrollment_change from
-- 202607280401_runner_protocol.sql.
CREATE OR REPLACE FUNCTION guard_runner_enrollment_change()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        IF NEW.revision <> 1
           OR NEW.state_kind NOT IN ('pending', 'active')
        THEN
            RAISE EXCEPTION
                'runner enrollment must begin pending or active at revision one'
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
       OR OLD.revision <> 1
       OR OLD.state_kind <> 'active'
       OR NEW.revision <> 2
       OR NEW.state_kind <> 'revoked'
    THEN
        RAISE EXCEPTION 'runner enrollment transition is not terminal revocation'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

-- One immutable admission record retains the predecessor and exact loss that
-- made a provisioning-only candidate admissible. The relation is deliberately
-- predecessor-scoped rather than a permanent deployment singleton.
CREATE TABLE runner_pending_enrollment (
    request_id uuid PRIMARY KEY,
    enrollment_id uuid NOT NULL UNIQUE,
    predecessor_enrollment_id uuid NOT NULL UNIQUE,
    predecessor_loss_epoch numeric(20, 0) NOT NULL,

    CONSTRAINT runner_pending_enrollment_distinct_runner
        CHECK (enrollment_id <> predecessor_enrollment_id),
    CONSTRAINT runner_pending_enrollment_positive_loss
        CHECK (
            predecessor_loss_epoch BETWEEN 1 AND 18446744073709551615
        ),
    CONSTRAINT runner_pending_enrollment_request_fk
        FOREIGN KEY (request_id, enrollment_id)
        REFERENCES runner_enrollment_request_receipt (
            request_id,
            enrollment_id
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT runner_pending_enrollment_candidate_fk
        FOREIGN KEY (enrollment_id)
        REFERENCES runner_enrollment (enrollment_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT runner_pending_enrollment_predecessor_loss_fk
        FOREIGN KEY (predecessor_enrollment_id, predecessor_loss_epoch)
        REFERENCES runner_connection_loss_epoch (enrollment_id, loss_epoch)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE TRIGGER runner_pending_enrollment_is_append_only
BEFORE UPDATE OR DELETE ON runner_pending_enrollment
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER runner_pending_enrollment_rejects_truncate
BEFORE TRUNCATE ON runner_pending_enrollment
FOR EACH STATEMENT
EXECUTE FUNCTION reject_immutable_record_change();

CREATE FUNCTION assert_runner_pending_enrollment_complete(
    checked_enrollment uuid
)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    candidate_state text;
    relation_count bigint;
    valid_relation_count bigint;
BEGIN
    SELECT state_kind
      INTO candidate_state
      FROM runner_enrollment
     WHERE enrollment_id = checked_enrollment;
    IF NOT FOUND THEN
        RETURN;
    END IF;

    SELECT count(*),
           count(*) FILTER (
               WHERE receipt.authority_kind = 'replacement_pending'
                 AND predecessor.state_kind = 'active'
                 AND authority.latest_loss_epoch =
                        pending.predecessor_loss_epoch
                 AND connection.state_kind = 'lost'
           )
      INTO relation_count, valid_relation_count
      FROM runner_pending_enrollment AS pending
      JOIN runner_enrollment_request_receipt AS receipt
        ON receipt.request_id = pending.request_id
       AND receipt.enrollment_id = pending.enrollment_id
      JOIN runner_enrollment AS predecessor
        ON predecessor.enrollment_id = pending.predecessor_enrollment_id
      JOIN runner_connection_authority_head AS authority
        ON authority.enrollment_id = pending.predecessor_enrollment_id
      JOIN runner_connection_event AS connection
        ON connection.enrollment_id = authority.enrollment_id
       AND connection.connection_epoch = authority.connection_epoch
       AND connection.event_ordinal = authority.connection_event_ordinal
     WHERE pending.enrollment_id = checked_enrollment;

    IF (candidate_state = 'pending'
            AND ROW(relation_count, valid_relation_count)
                IS DISTINCT FROM ROW(1::bigint, 1::bigint))
       OR (candidate_state <> 'pending' AND relation_count <> 0)
    THEN
        RAISE EXCEPTION
            'pending runner enrollment lacks exact lost predecessor authority'
            USING ERRCODE = '23514';
    END IF;
END;
$$;

CREATE FUNCTION require_runner_pending_enrollment_complete()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    PERFORM assert_runner_pending_enrollment_complete(
        COALESCE(NEW.enrollment_id, OLD.enrollment_id)
    );
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER runner_pending_enrollment_is_complete
AFTER INSERT ON runner_pending_enrollment
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_runner_pending_enrollment_complete();

CREATE CONSTRAINT TRIGGER runner_enrollment_pending_state_is_complete
AFTER INSERT OR UPDATE ON runner_enrollment
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_runner_pending_enrollment_complete();

CREATE CONSTRAINT TRIGGER runner_enrollment_receipt_pending_state_is_complete
AFTER INSERT ON runner_enrollment_request_receipt
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_runner_pending_enrollment_complete();

-- Supersedes guard_runner_registration_insert from
-- 202607280401_runner_protocol.sql. Pending authority may create only its
-- immutable first registration; later advertisement mutation remains refused.
CREATE OR REPLACE FUNCTION guard_runner_registration_insert()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    enrollment_state text;
    latest_revision numeric;
BEGIN
    SELECT state_kind INTO enrollment_state
      FROM runner_enrollment
     WHERE enrollment_id = NEW.enrollment_id
     FOR SHARE;
    SELECT max(registration_revision) INTO latest_revision
      FROM runner_registration
     WHERE enrollment_id = NEW.enrollment_id;
    IF NOT (
        (
            enrollment_state = 'active'
            AND NEW.registration_revision = COALESCE(latest_revision + 1, 1)
        )
        OR (
            enrollment_state = 'pending'
            AND latest_revision IS NULL
            AND NEW.registration_revision = 1
        )
    )
    THEN
        RAISE EXCEPTION 'runner registration lacks successor authority'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

-- Supersedes guard_runner_connection_event_insert from
-- 202608020006_runner_connection.sql. Pending authority admits a physical
-- connection for heartbeat and future command-bound provisioning only.
CREATE OR REPLACE FUNCTION guard_runner_connection_event_insert()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    prior runner_connection_event%ROWTYPE;
BEGIN
    PERFORM 1
      FROM runner_enrollment
     WHERE enrollment_id = NEW.enrollment_id
       AND state_kind IN ('pending', 'active')
     FOR UPDATE;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'runner connection requires non-revoked enrollment'
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

    IF NEW.connection_epoch < prior.connection_epoch
       OR (
            NEW.connection_epoch = prior.connection_epoch
            AND NEW.event_ordinal <> prior.event_ordinal + 1
       )
       OR (
            NEW.connection_epoch = prior.connection_epoch + 1
            AND (
                NEW.event_ordinal <> 1
                OR NEW.state_kind <> 'connected'
                OR NEW.cause_kind <> 'established'
            )
       )
       OR NEW.connection_epoch > prior.connection_epoch + 1
       OR (
            NEW.connection_epoch = prior.connection_epoch
            AND NOT (
                (prior.state_kind = 'connected'
                    AND NEW.state_kind IN ('suspect', 'shutdown', 'lost'))
                OR (prior.state_kind = 'suspect'
                    AND NEW.state_kind IN ('connected', 'shutdown', 'lost'))
            )
       )
    THEN
        RAISE EXCEPTION 'invalid runner connection transition'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;
