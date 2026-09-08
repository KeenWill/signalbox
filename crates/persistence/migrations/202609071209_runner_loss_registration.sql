ALTER TABLE runner_connection_loss_epoch
    ADD COLUMN registration_revision numeric(20, 0),
    ADD CONSTRAINT runner_connection_loss_registration_fk
        FOREIGN KEY (enrollment_id, registration_revision)
        REFERENCES runner_registration(enrollment_id, registration_revision);

CREATE OR REPLACE FUNCTION guard_runner_connection_loss_epoch() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    prior runner_connection_loss_epoch%ROWTYPE;
    source_state text;
BEGIN
    SELECT registration_revision INTO NEW.registration_revision
      FROM runner_current_registration WHERE enrollment_id = NEW.enrollment_id;
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
