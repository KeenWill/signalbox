ALTER TABLE runner_lease_event DROP CONSTRAINT runner_lease_event_state_shape;
ALTER TABLE runner_lease_event ADD CONSTRAINT runner_lease_event_state_shape CHECK (
    (event_ordinal = 1 AND state_kind = 'offered')
    OR (event_ordinal = 2 AND state_kind IN ('claimed', 'refused', 'lost_unclaimed', 'lost_execution_possible'))
    OR (event_ordinal = 3 AND state_kind IN ('completed', 'lost_claimed'))
);

CREATE TABLE runner_lease_failure (
    lease_id uuid NOT NULL,
    generation numeric(20,0) NOT NULL,
    category text NOT NULL CHECK (category IN (
        'credential_unavailable', 'repository_unavailable', 'sandbox_unavailable',
        'workspace_conflict', 'lease_admission_refused'
    )),
    detail jsonb NOT NULL CHECK (jsonb_typeof(detail) = 'object'),
    PRIMARY KEY (lease_id, generation),
    FOREIGN KEY (lease_id, generation) REFERENCES runner_lease_generation(lease_id, generation)
);
CREATE TRIGGER runner_lease_failure_is_append_only
    BEFORE UPDATE OR DELETE ON runner_lease_failure
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE FUNCTION require_runner_lease_refusal() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    current_state text;
    refusal_exists boolean;
BEGIN
    SELECT event.state_kind INTO current_state
        FROM runner_current_lease_event head
        JOIN runner_lease_event event USING (lease_id, generation, event_ordinal)
        WHERE head.lease_id = NEW.lease_id AND head.generation = NEW.generation;
    SELECT EXISTS (SELECT 1 FROM runner_lease_failure failure
        WHERE failure.lease_id = NEW.lease_id AND failure.generation = NEW.generation)
        INTO refusal_exists;
    IF (current_state = 'refused') IS DISTINCT FROM refusal_exists THEN
        RAISE EXCEPTION 'runner lease refusal requires its immutable evidence' USING ERRCODE = '23514';
    END IF;
    IF refusal_exists AND NOT EXISTS (
        SELECT 1 FROM runner_lease_generation lease
        JOIN tool_attempt attempt ON attempt.attempt_id = lease.attempt_id
        WHERE lease.lease_id = NEW.lease_id AND lease.generation = NEW.generation
            AND attempt.state_kind = 'terminal' AND attempt.terminal_disposition_kind = 'known_failed'
            AND attempt.error_kind = 'execution_failed' AND attempt.error_detail IS NULL
    ) THEN
        RAISE EXCEPTION 'runner lease refusal requires its failed physical attempt' USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;
CREATE CONSTRAINT TRIGGER runner_lease_refusal_has_evidence
    AFTER INSERT ON runner_lease_event DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION require_runner_lease_refusal();
CREATE CONSTRAINT TRIGGER runner_lease_failure_settles_authority
    AFTER INSERT ON runner_lease_failure DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION require_runner_lease_refusal();
