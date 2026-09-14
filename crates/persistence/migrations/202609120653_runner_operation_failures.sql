ALTER TABLE runner_lease_event DROP CONSTRAINT runner_lease_event_state_shape;
ALTER TABLE runner_lease_event ADD CONSTRAINT runner_lease_event_state_shape CHECK (
    (event_ordinal = 1 AND state_kind = 'offered')
    OR (event_ordinal = 2 AND state_kind IN ('claimed', 'refused', 'lost_unclaimed', 'lost_execution_possible'))
    OR (event_ordinal = 3 AND state_kind IN ('completed', 'lost_claimed', 'refused'))
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
CREATE TRIGGER runner_lease_failure_rejects_truncate
    BEFORE TRUNCATE ON runner_lease_failure
    FOR EACH STATEMENT EXECUTE FUNCTION reject_immutable_record_change();

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

CREATE OR REPLACE FUNCTION guard_runner_lease_event() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE prior_state text;
BEGIN
    IF NEW.event_ordinal = 1 THEN RETURN NEW; END IF;
    SELECT state_kind INTO prior_state FROM runner_lease_event
        WHERE lease_id = NEW.lease_id AND generation = NEW.generation
            AND event_ordinal = NEW.event_ordinal - 1;
    IF NOT FOUND
        OR (NEW.event_ordinal = 2 AND prior_state <> 'offered')
        OR (NEW.event_ordinal = 3 AND NOT (
            (NEW.state_kind = 'refused' AND prior_state = 'lost_unclaimed')
            OR (NEW.state_kind IN ('completed', 'lost_claimed') AND prior_state = 'claimed')
        )) THEN
        RAISE EXCEPTION 'runner lease event transition is not monotonic' USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE OR REPLACE FUNCTION assert_runner_no_execution_proof_complete(checked_lease uuid, checked_generation numeric)
RETURNS void LANGUAGE plpgsql AS $$
BEGIN
    IF EXISTS (SELECT 1 FROM runner_lease_no_execution_proof
        WHERE lease_id = checked_lease AND generation = checked_generation)
        IS DISTINCT FROM EXISTS (SELECT 1 FROM runner_lease_event
        WHERE lease_id = checked_lease AND generation = checked_generation AND state_kind = 'lost_unclaimed') THEN
        RAISE EXCEPTION 'runner lost-unclaimed lease lacks exact no-execution proof' USING ERRCODE = '23514';
    END IF;
END;
$$;

DO $migration$
DECLARE
    function_name text;
    definition text;
    prior text := $prior$                    OR (
                        attempt.state_kind = 'terminal'
                        AND attempt.terminal_disposition_kind = 'ambiguous'
                        AND lease_event.state_kind IN ($prior$;
    refusal text := $refusal$                    OR (
                        attempt.state_kind = 'terminal'
                        AND attempt.terminal_disposition_kind = 'known_failed'
                        AND attempt.error_kind = 'execution_failed'
                        AND attempt.error_detail IS NULL
                        AND lease_event.state_kind = 'refused'
                        AND EXISTS (SELECT 1 FROM runner_lease_failure failure
                            WHERE failure.lease_id = lease.lease_id AND failure.generation = lease.generation)
                    )
$refusal$;
BEGIN
    FOREACH function_name IN ARRAY ARRAY[
        'assert_turn_runner_recovery_complete(uuid,uuid)',
        'assert_runner_placement_interrupted_attempt_complete(uuid,numeric)'
    ] LOOP
        SELECT pg_get_functiondef(function_name::regprocedure) INTO definition;
        IF strpos(definition, prior) = 0 THEN
            RAISE EXCEPTION 'runner recovery attempt predicate is missing from %', function_name;
        END IF;
        EXECUTE replace(definition, prior, refusal || prior);
    END LOOP;
END;
$migration$;
