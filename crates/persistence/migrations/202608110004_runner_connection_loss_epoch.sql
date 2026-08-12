-- Fence terminal runner connections with an append-only durable loss epoch.

CREATE TABLE runner_connection_loss_epoch (
    enrollment_id uuid NOT NULL,
    loss_epoch numeric(20, 0) NOT NULL,
    connection_epoch numeric(20, 0) NOT NULL,
    connection_event_ordinal numeric(20, 0) NOT NULL,

    CONSTRAINT runner_connection_loss_epoch_pk
        PRIMARY KEY (enrollment_id, loss_epoch),
    CONSTRAINT runner_connection_loss_epoch_source_key
        UNIQUE (enrollment_id, connection_epoch),
    CONSTRAINT runner_connection_loss_epoch_positive_u64 CHECK (
        loss_epoch BETWEEN 1 AND 18446744073709551615
        AND connection_epoch BETWEEN 1 AND 18446744073709551615
        AND connection_event_ordinal BETWEEN 1 AND 18446744073709551615
    ),
    CONSTRAINT runner_connection_loss_epoch_source_fk
        FOREIGN KEY (
            enrollment_id,
            connection_epoch,
            connection_event_ordinal
        )
        REFERENCES runner_connection_event (
            enrollment_id,
            connection_epoch,
            event_ordinal
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
);

INSERT INTO runner_connection_loss_epoch (
    enrollment_id,
    loss_epoch,
    connection_epoch,
    connection_event_ordinal
)
SELECT enrollment_id,
       row_number() OVER (
           PARTITION BY enrollment_id
           ORDER BY connection_epoch
       ),
       connection_epoch,
       event_ordinal
  FROM runner_connection_event
 WHERE state_kind = 'lost'
 ORDER BY enrollment_id, connection_epoch;

DO $migration$
BEGIN
    IF EXISTS (
        SELECT 1
          FROM runner_connection_loss_epoch AS loss
          JOIN runner_connection_event AS connection
            ON connection.enrollment_id = loss.enrollment_id
           AND connection.connection_epoch = loss.connection_epoch
           AND connection.event_ordinal = loss.connection_event_ordinal
         WHERE connection.state_kind <> 'lost'
            OR loss.loss_epoch <> (
                SELECT count(*)
                  FROM runner_connection_event AS prior
                 WHERE prior.enrollment_id = loss.enrollment_id
                   AND prior.state_kind = 'lost'
                   AND prior.connection_epoch <= loss.connection_epoch
            )
    ) THEN
        RAISE EXCEPTION 'runner connection loss backfill is not canonical';
    END IF;
END;
$migration$;

CREATE FUNCTION guard_runner_connection_loss_epoch()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    prior runner_connection_loss_epoch%ROWTYPE;
    source_state text;
BEGIN
    SELECT state_kind
      INTO source_state
      FROM runner_connection_event
     WHERE enrollment_id = NEW.enrollment_id
       AND connection_epoch = NEW.connection_epoch
       AND event_ordinal = NEW.connection_event_ordinal;
    SELECT *
      INTO prior
      FROM runner_connection_loss_epoch
     WHERE enrollment_id = NEW.enrollment_id
     ORDER BY loss_epoch DESC
     LIMIT 1;
    IF source_state IS DISTINCT FROM 'lost'
       OR (
            NOT FOUND
            AND NEW.loss_epoch <> 1
       )
       OR (
            prior.enrollment_id IS NOT NULL
            AND (
                NEW.loss_epoch <> prior.loss_epoch + 1
                OR NEW.connection_epoch <= prior.connection_epoch
            )
       )
    THEN
        RAISE EXCEPTION 'runner loss epoch lacks its next terminal connection'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER runner_connection_loss_epoch_is_guarded
BEFORE INSERT ON runner_connection_loss_epoch
FOR EACH ROW
EXECUTE FUNCTION guard_runner_connection_loss_epoch();

CREATE TRIGGER runner_connection_loss_epoch_is_append_only
BEFORE UPDATE OR DELETE ON runner_connection_loss_epoch
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER runner_connection_loss_epoch_rejects_truncate
BEFORE TRUNCATE ON runner_connection_loss_epoch
FOR EACH STATEMENT
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TABLE runner_current_connection_loss (
    enrollment_id uuid PRIMARY KEY,
    loss_epoch numeric(20, 0) NOT NULL,

    CONSTRAINT runner_current_connection_loss_fk
        FOREIGN KEY (enrollment_id, loss_epoch)
        REFERENCES runner_connection_loss_epoch (enrollment_id, loss_epoch)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

INSERT INTO runner_current_connection_loss (enrollment_id, loss_epoch)
SELECT enrollment_id, max(loss_epoch)
  FROM runner_connection_loss_epoch
 GROUP BY enrollment_id;

CREATE FUNCTION guard_runner_current_connection_loss()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    latest_epoch numeric;
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'runner loss head cannot be deleted'
            USING ERRCODE = '23514';
    END IF;
    SELECT max(loss_epoch)
      INTO latest_epoch
      FROM runner_connection_loss_epoch
     WHERE enrollment_id = NEW.enrollment_id;
    IF NEW.loss_epoch IS DISTINCT FROM latest_epoch
       OR (
            TG_OP = 'INSERT'
            AND NEW.loss_epoch <> 1
       )
       OR (
            TG_OP = 'UPDATE'
            AND (
                OLD.enrollment_id <> NEW.enrollment_id
                OR NEW.loss_epoch <> OLD.loss_epoch + 1
            )
       )
    THEN
        RAISE EXCEPTION 'runner loss head must advance to the latest epoch'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER runner_current_connection_loss_advances
BEFORE INSERT OR UPDATE OR DELETE ON runner_current_connection_loss
FOR EACH ROW
EXECUTE FUNCTION guard_runner_current_connection_loss();

CREATE TRIGGER runner_current_connection_loss_rejects_truncate
BEFORE TRUNCATE ON runner_current_connection_loss
FOR EACH STATEMENT
EXECUTE FUNCTION reject_immutable_record_change();

-- This row exists before loss and is the short transaction's serialization
-- point. The latest loss epoch is retained across a later connected epoch.
CREATE TABLE runner_connection_authority_head (
    enrollment_id uuid PRIMARY KEY,
    connection_epoch numeric(20, 0) NOT NULL,
    connection_event_ordinal numeric(20, 0) NOT NULL,
    latest_loss_epoch numeric(20, 0),

    CONSTRAINT runner_connection_authority_head_positive_u64 CHECK (
        connection_epoch BETWEEN 1 AND 18446744073709551615
        AND connection_event_ordinal BETWEEN 1 AND 18446744073709551615
        AND (
            latest_loss_epoch IS NULL
            OR latest_loss_epoch BETWEEN 1 AND 18446744073709551615
        )
    ),
    CONSTRAINT runner_connection_authority_head_connection_fk
        FOREIGN KEY (
            enrollment_id,
            connection_epoch,
            connection_event_ordinal
        )
        REFERENCES runner_connection_event (
            enrollment_id,
            connection_epoch,
            event_ordinal
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT runner_connection_authority_head_loss_fk
        FOREIGN KEY (enrollment_id, latest_loss_epoch)
        REFERENCES runner_connection_loss_epoch (enrollment_id, loss_epoch)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

INSERT INTO runner_connection_authority_head (
    enrollment_id,
    connection_epoch,
    connection_event_ordinal,
    latest_loss_epoch
)
SELECT DISTINCT ON (connection.enrollment_id)
       connection.enrollment_id,
       connection.connection_epoch,
       connection.event_ordinal,
       current_loss.loss_epoch
  FROM runner_connection_event AS connection
  LEFT JOIN runner_current_connection_loss AS current_loss
    ON current_loss.enrollment_id = connection.enrollment_id
 ORDER BY connection.enrollment_id,
          connection.connection_epoch DESC,
          connection.event_ordinal DESC;

CREATE FUNCTION guard_runner_connection_authority_head()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF TG_OP = 'DELETE'
       OR (
            TG_OP = 'UPDATE'
            AND (
                OLD.enrollment_id <> NEW.enrollment_id
                OR NEW.connection_epoch < OLD.connection_epoch
                OR (
                    NEW.connection_epoch = OLD.connection_epoch
                    AND NEW.connection_event_ordinal <=
                        OLD.connection_event_ordinal
                )
                OR (
                    NEW.connection_epoch > OLD.connection_epoch
                    AND NEW.connection_event_ordinal <> 1
                )
                OR (
                    NEW.latest_loss_epoch IS DISTINCT FROM
                        OLD.latest_loss_epoch
                    AND NEW.latest_loss_epoch IS NULL
                )
            )
       )
    THEN
        RAISE EXCEPTION 'runner connection authority head must advance monotonically'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER runner_connection_authority_head_advances
BEFORE INSERT OR UPDATE OR DELETE ON runner_connection_authority_head
FOR EACH ROW
EXECUTE FUNCTION guard_runner_connection_authority_head();

CREATE TRIGGER runner_connection_authority_head_rejects_truncate
BEFORE TRUNCATE ON runner_connection_authority_head
FOR EACH STATEMENT
EXECUTE FUNCTION reject_immutable_record_change();

CREATE FUNCTION assert_runner_connection_authority_head_complete(
    checked_enrollment uuid
)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    expected_connection_epoch numeric;
    expected_connection_event_ordinal numeric;
    expected_loss_epoch numeric;
    current_loss_epoch numeric;
    head runner_connection_authority_head%ROWTYPE;
BEGIN
    SELECT connection_epoch, event_ordinal
      INTO expected_connection_epoch, expected_connection_event_ordinal
      FROM runner_connection_event
     WHERE enrollment_id = checked_enrollment
     ORDER BY connection_epoch DESC, event_ordinal DESC
     LIMIT 1;
    SELECT max(loss_epoch)
      INTO expected_loss_epoch
      FROM runner_connection_loss_epoch
     WHERE enrollment_id = checked_enrollment;
    SELECT loss_epoch
      INTO current_loss_epoch
      FROM runner_current_connection_loss
     WHERE enrollment_id = checked_enrollment;
    SELECT *
      INTO head
      FROM runner_connection_authority_head
     WHERE enrollment_id = checked_enrollment;
    IF expected_loss_epoch IS DISTINCT FROM current_loss_epoch
       OR (
            expected_connection_epoch IS NOT NULL
            AND ROW(
                head.connection_epoch,
                head.connection_event_ordinal,
                head.latest_loss_epoch
            ) IS DISTINCT FROM ROW(
                expected_connection_epoch,
                expected_connection_event_ordinal,
                expected_loss_epoch
            )
       )
    THEN
        RAISE EXCEPTION 'runner connection authority head is not complete'
            USING ERRCODE = '23514';
    END IF;
END;
$$;

CREATE FUNCTION require_runner_connection_authority_head_complete()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    PERFORM assert_runner_connection_authority_head_complete(NEW.enrollment_id);
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER runner_connection_event_rechecks_authority_head
AFTER INSERT ON runner_connection_event
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_runner_connection_authority_head_complete();

CREATE CONSTRAINT TRIGGER runner_connection_loss_rechecks_authority_head
AFTER INSERT ON runner_connection_loss_epoch
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_runner_connection_authority_head_complete();

CREATE CONSTRAINT TRIGGER runner_connection_authority_head_is_complete
AFTER INSERT OR UPDATE ON runner_connection_authority_head
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_runner_connection_authority_head_complete();

CREATE FUNCTION assert_runner_connection_loss_complete(
    checked_enrollment uuid,
    checked_connection_epoch numeric
)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    connection_state text;
    loss_count bigint;
BEGIN
    SELECT state_kind
      INTO connection_state
      FROM runner_connection_event
     WHERE enrollment_id = checked_enrollment
       AND connection_epoch = checked_connection_epoch
     ORDER BY event_ordinal DESC
     LIMIT 1;
    SELECT count(*)
      INTO loss_count
      FROM runner_connection_loss_epoch
     WHERE enrollment_id = checked_enrollment
       AND connection_epoch = checked_connection_epoch;
    IF (connection_state = 'lost' AND loss_count <> 1)
       OR (connection_state IS DISTINCT FROM 'lost' AND loss_count <> 0)
    THEN
        RAISE EXCEPTION 'terminal runner connection lacks its exact loss epoch'
            USING ERRCODE = '23514';
    END IF;
END;
$$;

CREATE FUNCTION require_runner_connection_loss_complete()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    PERFORM assert_runner_connection_loss_complete(
        NEW.enrollment_id,
        NEW.connection_epoch
    );
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER runner_connection_event_loss_is_complete
AFTER INSERT ON runner_connection_event
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_runner_connection_loss_complete();

CREATE CONSTRAINT TRIGGER runner_connection_loss_epoch_is_complete
AFTER INSERT ON runner_connection_loss_epoch
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_runner_connection_loss_complete();

SET CONSTRAINTS ALL IMMEDIATE;

ALTER TABLE runner_lease_generation
    DISABLE TRIGGER runner_lease_generation_is_append_only;

ALTER TABLE runner_lease_generation
    ADD COLUMN offer_connection_epoch numeric(20, 0),
    ADD COLUMN offer_connection_event_ordinal numeric(20, 0),
    ADD COLUMN offer_loss_epoch numeric(20, 0),
    ADD CONSTRAINT runner_lease_offer_connection_shape CHECK (
        (
            offer_connection_epoch IS NULL
            AND offer_connection_event_ordinal IS NULL
            AND offer_loss_epoch IS NULL
        )
        OR (
            offer_connection_epoch BETWEEN 1 AND 18446744073709551615
            AND offer_connection_event_ordinal
                BETWEEN 1 AND 18446744073709551615
            AND (
                offer_loss_epoch IS NULL
                OR offer_loss_epoch BETWEEN 1 AND 18446744073709551615
            )
        )
    ),
    ADD CONSTRAINT runner_lease_offer_connection_fk
        FOREIGN KEY (
            registration_enrollment_id,
            offer_connection_epoch,
            offer_connection_event_ordinal
        )
        REFERENCES runner_connection_event (
            enrollment_id,
            connection_epoch,
            event_ordinal
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    ADD CONSTRAINT runner_lease_offer_loss_fk
        FOREIGN KEY (registration_enrollment_id, offer_loss_epoch)
        REFERENCES runner_connection_loss_epoch (enrollment_id, loss_epoch)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED;

DO $migration$
BEGIN
    IF EXISTS (
        SELECT 1
          FROM runner_lease_generation AS generation
          JOIN runner_current_lease_event AS current_lease
            ON current_lease.lease_id = generation.lease_id
           AND current_lease.generation = generation.generation
          JOIN runner_lease_event AS lease_event
            ON lease_event.lease_id = current_lease.lease_id
           AND lease_event.generation = current_lease.generation
           AND lease_event.event_ordinal = current_lease.event_ordinal
         WHERE lease_event.state_kind = 'offered'
    ) THEN
        RAISE EXCEPTION
            'outstanding runner lease lacks reconstructible offer authority';
    END IF;
END;
$migration$;

UPDATE runner_lease_generation AS generation
   SET offer_connection_epoch = authority.connection_epoch,
       offer_connection_event_ordinal = authority.connection_event_ordinal,
       offer_loss_epoch = authority.latest_loss_epoch
  FROM runner_connection_authority_head AS authority
 WHERE authority.enrollment_id = generation.registration_enrollment_id;

ALTER TABLE runner_lease_generation
    ENABLE TRIGGER runner_lease_generation_is_append_only;

CREATE FUNCTION reject_runner_lease_generation_after_connection_loss()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    authority runner_connection_authority_head%ROWTYPE;
    connection runner_connection_event%ROWTYPE;
    loss runner_connection_loss_epoch%ROWTYPE;
BEGIN
    IF NEW.offer_connection_epoch IS NOT NULL
       OR NEW.offer_connection_event_ordinal IS NOT NULL
       OR NEW.offer_loss_epoch IS NOT NULL
    THEN
        RAISE EXCEPTION 'runner lease offer authority is adapter-owned'
            USING ERRCODE = '23514';
    END IF;
    PERFORM 1
      FROM runner_enrollment
     WHERE enrollment_id = NEW.registration_enrollment_id
     FOR SHARE;
    SELECT *
      INTO authority
      FROM runner_connection_authority_head
     WHERE enrollment_id = NEW.registration_enrollment_id
     FOR SHARE;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'runner lease offer lacks connection authority'
            USING ERRCODE = '23514';
    END IF;
    SELECT *
      INTO connection
      FROM runner_connection_event
     WHERE enrollment_id = authority.enrollment_id
       AND connection_epoch = authority.connection_epoch
       AND event_ordinal = authority.connection_event_ordinal;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'runner connection authority head lacks its event'
            USING ERRCODE = '23514';
    END IF;
    IF connection.state_kind = 'shutdown' THEN
        RAISE EXCEPTION 'shutdown runner connection cannot authorize a lease offer'
            USING ERRCODE = '23514';
    END IF;
    IF connection.state_kind IS DISTINCT FROM 'lost' THEN
        NEW.offer_connection_epoch := authority.connection_epoch;
        NEW.offer_connection_event_ordinal :=
            authority.connection_event_ordinal;
        NEW.offer_loss_epoch := authority.latest_loss_epoch;
        RETURN NEW;
    END IF;
    SELECT epoch.*
      INTO loss
      FROM runner_current_connection_loss AS current_loss
      JOIN runner_connection_loss_epoch AS epoch
        ON epoch.enrollment_id = current_loss.enrollment_id
       AND epoch.loss_epoch = current_loss.loss_epoch
     WHERE current_loss.enrollment_id = authority.enrollment_id
     FOR SHARE OF current_loss;
    IF authority.latest_loss_epoch IS DISTINCT FROM loss.loss_epoch
       OR loss.connection_epoch IS DISTINCT FROM connection.connection_epoch
       OR loss.connection_event_ordinal IS DISTINCT FROM
            connection.event_ordinal
    THEN
        RAISE EXCEPTION 'lost runner connection lacks its current epoch fence'
            USING ERRCODE = '23514';
    END IF;
    RAISE EXCEPTION 'lost runner connection cannot authorize a lease offer'
        USING ERRCODE = '23514';
END;
$$;

CREATE TRIGGER runner_lease_generation_connection_loss_fence
BEFORE INSERT ON runner_lease_generation
FOR EACH ROW
EXECUTE FUNCTION reject_runner_lease_generation_after_connection_loss();

CREATE FUNCTION reject_runner_lease_claim_after_connection_loss()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    offered runner_lease_generation%ROWTYPE;
    authority runner_connection_authority_head%ROWTYPE;
    connection_state text;
BEGIN
    IF NEW.state_kind <> 'claimed' THEN
        RETURN NEW;
    END IF;
    SELECT *
      INTO offered
      FROM runner_lease_generation AS lease_generation
     WHERE lease_generation.lease_id = NEW.lease_id
       AND lease_generation.generation = NEW.generation;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'runner lease claim lacks its generation'
            USING ERRCODE = '23514';
    END IF;
    PERFORM 1
      FROM runner_enrollment
     WHERE enrollment_id = offered.registration_enrollment_id
     FOR SHARE;
    SELECT *
      INTO authority
      FROM runner_connection_authority_head
     WHERE enrollment_id = offered.registration_enrollment_id
     FOR SHARE;
    IF authority.enrollment_id IS NULL THEN
        RAISE EXCEPTION 'runner lease claim lacks connection authority'
            USING ERRCODE = '23514';
    END IF;
    IF offered.offer_connection_epoch IS NULL THEN
        RAISE EXCEPTION 'runner lease claim lacks offer connection authority'
            USING ERRCODE = '23514';
    END IF;
    IF authority.connection_epoch IS DISTINCT FROM
            offered.offer_connection_epoch
       OR authority.latest_loss_epoch IS DISTINCT FROM
            offered.offer_loss_epoch
    THEN
        RAISE EXCEPTION 'runner lease claim crossed a connection loss fence'
            USING ERRCODE = '23514';
    END IF;
    SELECT state_kind
      INTO connection_state
      FROM runner_connection_event
     WHERE enrollment_id = authority.enrollment_id
       AND connection_epoch = authority.connection_epoch
       AND event_ordinal = authority.connection_event_ordinal;
    IF connection_state IS DISTINCT FROM 'connected' THEN
        RAISE EXCEPTION 'runner lease claim lacks a live connection'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER runner_lease_claim_connection_loss_fence
BEFORE INSERT ON runner_lease_event
FOR EACH ROW
EXECUTE FUNCTION reject_runner_lease_claim_after_connection_loss();
