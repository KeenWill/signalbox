ALTER TABLE credential_availability_wait
    DROP CONSTRAINT credential_availability_wait_cause_check,
    ADD CONSTRAINT credential_availability_wait_cause_check CHECK (cause IN ('contended', 'exhausted'));

CREATE TABLE credential_invocation_capacity (
    profile text PRIMARY KEY,
    max_concurrent_invocations integer CHECK (max_concurrent_invocations > 0),
    registered boolean NOT NULL
);
CREATE TABLE credential_invocation_reservation (
    model_call_id uuid PRIMARY KEY REFERENCES model_call,
    profile text NOT NULL REFERENCES credential_invocation_capacity,
    process_group_id bigint CHECK (process_group_id > 0 AND process_group_id <= 4294967295),
    released_at timestamptz
);
CREATE INDEX credential_invocation_reservation_active ON credential_invocation_reservation (profile)
    WHERE released_at IS NULL;
ALTER TABLE credential_availability_wait_member
    ADD COLUMN capacity_bound integer CHECK (capacity_bound > 0),
    ADD COLUMN reservation_ids uuid[] NOT NULL DEFAULT '{}',
    ADD CONSTRAINT credential_wait_capacity_evidence CHECK (
        (capacity_bound IS NULL AND cardinality(reservation_ids) = 0) OR
        (capacity_bound IS NOT NULL AND cardinality(reservation_ids) >= capacity_bound AND jsonb_array_length(exclusions) = 0)
    );

CREATE FUNCTION release_credential_invocation(checked_call uuid) RETURNS void LANGUAGE plpgsql AS $$
DECLARE checked_profile text;
BEGIN
    SELECT profile INTO checked_profile FROM credential_invocation_reservation
      WHERE model_call_id = checked_call AND released_at IS NULL;
    IF NOT FOUND THEN RETURN; END IF;
    PERFORM 1 FROM credential_invocation_capacity WHERE profile = checked_profile FOR UPDATE;
    UPDATE credential_invocation_reservation SET released_at = clock_timestamp()
      WHERE model_call_id = checked_call AND released_at IS NULL;
    IF FOUND THEN
        UPDATE credential_availability_wait waiting SET eligible = true
         WHERE waiting.consumed_by_attempt_id IS NULL AND waiting.cause = 'contended' AND NOT waiting.eligible
           AND EXISTS (SELECT 1 FROM credential_availability_wait_member member
             WHERE member.wait_attempt_id = waiting.wait_attempt_id AND member.profile = checked_profile
               AND member.capacity_bound IS NOT NULL);
    END IF;
END;
$$;

CREATE FUNCTION release_uninvoked_credential_reservation() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.state_kind = 'prepared' AND NEW.state_kind = 'terminal' THEN
        PERFORM release_credential_invocation(NEW.model_call_id);
    END IF;
    RETURN NULL;
END;
$$;
CREATE TRIGGER credential_invocation_uninvoked_terminal AFTER UPDATE OF state_kind ON model_call
    FOR EACH ROW EXECUTE FUNCTION release_uninvoked_credential_reservation();

ALTER FUNCTION assert_credential_availability_wait(uuid) RENAME TO assert_credential_availability_wait_before_capacity;
CREATE FUNCTION assert_credential_availability_wait(checked_turn uuid) RETURNS void LANGUAGE plpgsql AS $$
DECLARE waiting credential_availability_wait%ROWTYPE;
BEGIN
    PERFORM assert_credential_availability_wait_before_capacity(checked_turn);
    SELECT * INTO STRICT waiting FROM credential_availability_wait
      WHERE turn_id = checked_turn AND consumed_by_attempt_id IS NULL;
    IF (waiting.cause = 'contended') IS DISTINCT FROM EXISTS (
        SELECT 1 FROM credential_availability_wait_member WHERE wait_attempt_id = waiting.wait_attempt_id AND capacity_bound IS NOT NULL)
       OR EXISTS (SELECT 1 FROM credential_availability_wait_member member
          WHERE member.wait_attempt_id = waiting.wait_attempt_id AND (
            (jsonb_array_length(member.exclusions) = 0 AND member.capacity_bound IS NULL) OR
            cardinality(member.reservation_ids) <> (SELECT count(DISTINCT reservation.model_call_id)
              FROM credential_invocation_reservation reservation WHERE reservation.profile = member.profile
                AND reservation.model_call_id = ANY(member.reservation_ids)))) THEN
        RAISE EXCEPTION 'credential wait capacity evidence is incomplete' USING ERRCODE = '23514';
    END IF;
END;
$$;

CREATE FUNCTION guard_credential_invocation_reservation() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        PERFORM 1 FROM credential_invocation_capacity WHERE profile = NEW.profile FOR UPDATE;
        IF EXISTS (SELECT 1 FROM credential_invocation_capacity capacity WHERE capacity.profile = NEW.profile
            AND capacity.max_concurrent_invocations <= (SELECT count(*) FROM credential_invocation_reservation
                WHERE profile = NEW.profile AND released_at IS NULL)) THEN
            RAISE EXCEPTION 'credential invocation bound is saturated' USING ERRCODE = '23514';
        END IF;
        IF NOT EXISTS (SELECT 1 FROM model_call WHERE model_call_id = NEW.model_call_id
            AND credential_reference = NEW.profile AND state_kind = 'prepared') THEN
            RAISE EXCEPTION 'invocation reservation lacks selected prepared call' USING ERRCODE = '23514';
        END IF;
    ELSIF TG_OP = 'DELETE' OR OLD.released_at IS NOT NULL
       OR (NEW.model_call_id, NEW.profile) IS DISTINCT FROM (OLD.model_call_id, OLD.profile)
       OR (OLD.process_group_id IS NOT NULL AND NEW.process_group_id IS DISTINCT FROM OLD.process_group_id) THEN
        RAISE EXCEPTION 'invocation reservation identity is immutable' USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER credential_invocation_reservation_identity BEFORE INSERT OR UPDATE OR DELETE ON credential_invocation_reservation
    FOR EACH ROW EXECUTE FUNCTION guard_credential_invocation_reservation();
