-- A placement retains the registration revision that created its immutable
-- snapshot. Every lease offer independently retains the then-current
-- registration revision that revalidated that snapshot for dispatch.

ALTER TABLE runner_lease_generation
    DISABLE TRIGGER runner_lease_generation_is_append_only;

ALTER TABLE runner_lease_generation
    ADD COLUMN IF NOT EXISTS offer_registration_revision numeric(20, 0);

UPDATE runner_lease_generation
   SET offer_registration_revision = registration_revision;

ALTER TABLE runner_lease_generation
    ALTER COLUMN offer_registration_revision SET NOT NULL,
    ADD CONSTRAINT runner_lease_offer_registration_positive_u64
        CHECK (
            offer_registration_revision
                BETWEEN 1 AND 18446744073709551615
        ),
    ADD CONSTRAINT runner_lease_offer_registration_fk
        FOREIGN KEY (
            registration_enrollment_id,
            offer_registration_revision
        )
        REFERENCES runner_registration (
            enrollment_id,
            registration_revision
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED;

ALTER TABLE runner_lease_generation
    ENABLE TRIGGER runner_lease_generation_is_append_only;

CREATE FUNCTION reject_runner_lease_offer_from_stale_registration()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    current_revision numeric;
BEGIN
    IF NEW.offer_registration_revision IS NULL THEN
        NEW.offer_registration_revision := NEW.registration_revision;
    END IF;
    SELECT registration_revision
      INTO current_revision
      FROM runner_current_registration
     WHERE enrollment_id = NEW.registration_enrollment_id
       FOR SHARE;
    IF current_revision IS DISTINCT FROM NEW.offer_registration_revision THEN
        RAISE EXCEPTION
            'runner lease offer must name the current registration revision'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER runner_lease_generation_offer_registration_is_guarded
BEFORE INSERT ON runner_lease_generation
FOR EACH ROW
EXECUTE FUNCTION reject_runner_lease_offer_from_stale_registration();
