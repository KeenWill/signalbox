-- Lease execution records the current offer authority independently of the
-- immutable registration snapshot retained by the pinned placement.
ALTER TABLE runner_lease_generation
    ADD COLUMN offer_registration_revision numeric(20,0) NOT NULL,
    ADD CONSTRAINT runner_lease_offer_registration_tool_fk
        FOREIGN KEY (registration_enrollment_id, offer_registration_revision, tool_name)
        REFERENCES runner_registration_tool (enrollment_id, registration_revision, tool_name)
        ON UPDATE RESTRICT ON DELETE RESTRICT;

CREATE FUNCTION capture_runner_lease_offer_registration() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    SELECT registration_revision INTO NEW.offer_registration_revision
      FROM runner_current_registration
     WHERE enrollment_id = NEW.registration_enrollment_id
       FOR SHARE;
    IF NEW.offer_registration_revision IS NULL THEN
        RAISE EXCEPTION 'runner lease requires current offer registration'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER runner_lease_offer_registration_is_captured
    BEFORE INSERT ON runner_lease_generation
    FOR EACH ROW EXECUTE FUNCTION capture_runner_lease_offer_registration();
