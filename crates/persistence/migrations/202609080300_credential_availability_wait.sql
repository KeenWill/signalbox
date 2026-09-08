ALTER TABLE credential_pool_availability_successor ADD COLUMN non_acceptance_proven boolean NOT NULL;

CREATE TABLE session_credential_pool_policy (
    session_id uuid NOT NULL REFERENCES session,
    effective_target_id uuid NOT NULL,
    pool_policy_id uuid NOT NULL REFERENCES credential_pool_policy,
    PRIMARY KEY (session_id, effective_target_id)
);
CREATE TRIGGER session_credential_pool_policy_immutable BEFORE UPDATE OR DELETE ON session_credential_pool_policy
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE TABLE credential_availability_wait (
    wait_attempt_id uuid PRIMARY KEY REFERENCES turn_attempt,
    session_id uuid NOT NULL REFERENCES session,
    turn_id uuid NOT NULL REFERENCES turn_lifecycle,
    frontier_id uuid NOT NULL,
    pool_policy_id uuid NOT NULL REFERENCES credential_pool_policy,
    effective_target_id uuid NOT NULL,
    cause text NOT NULL CHECK (cause IN ('contended', 'exhausted')),
    deadline timestamptz,
    eligible boolean NOT NULL DEFAULT false,
    predecessor_model_call_id uuid REFERENCES model_call,
    predecessor_non_acceptance_proven boolean,
    CHECK ((predecessor_model_call_id IS NULL) = (predecessor_non_acceptance_proven IS NULL)),
    consumed_by_attempt_id uuid UNIQUE REFERENCES turn_attempt,
    FOREIGN KEY (session_id, frontier_id) REFERENCES context_frontier (owning_session_id, context_frontier_id),
    UNIQUE (wait_attempt_id, pool_policy_id)
);
CREATE UNIQUE INDEX credential_availability_wait_one_active_per_turn ON credential_availability_wait (turn_id)
    WHERE consumed_by_attempt_id IS NULL;
CREATE INDEX credential_availability_wait_deadline ON credential_availability_wait (deadline)
    WHERE consumed_by_attempt_id IS NULL AND deadline IS NOT NULL;
CREATE TABLE credential_availability_wait_member (
    wait_attempt_id uuid NOT NULL,
    pool_policy_id uuid NOT NULL,
    ordinal integer NOT NULL,
    profile text NOT NULL,
    exclusions jsonb NOT NULL CHECK (jsonb_typeof(exclusions) = 'array'),
    PRIMARY KEY (wait_attempt_id, ordinal),
    FOREIGN KEY (wait_attempt_id, pool_policy_id) REFERENCES credential_availability_wait (wait_attempt_id, pool_policy_id),
    FOREIGN KEY (pool_policy_id, ordinal) REFERENCES credential_pool_policy_member (pool_policy_id, ordinal)
);
CREATE TABLE credential_availability_wait_release (
    turn_attempt_id uuid PRIMARY KEY REFERENCES turn_attempt,
    wait_attempt_id uuid NOT NULL UNIQUE REFERENCES credential_availability_wait
);
CREATE TRIGGER credential_availability_wait_release_immutable BEFORE UPDATE OR DELETE ON credential_availability_wait_release
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

DO $$ DECLARE definition text; BEGIN
    SELECT pg_get_constraintdef(oid) INTO definition FROM pg_constraint
        WHERE conrelid = 'turn_lifecycle'::regclass AND conname = 'turn_lifecycle_active_phase_closed';
    ALTER TABLE turn_lifecycle DROP CONSTRAINT turn_lifecycle_active_phase_closed;
    EXECUTE 'ALTER TABLE turn_lifecycle ADD CONSTRAINT turn_lifecycle_active_phase_closed CHECK (('
        || substr(definition, 8, length(definition) - 8) || ') OR active_phase_kind = ''awaiting_credential_availability'')';
    SELECT pg_get_constraintdef(oid) INTO definition FROM pg_constraint
        WHERE conrelid = 'turn_lifecycle'::regclass AND conname = 'turn_lifecycle_state_payload_shape';
    ALTER TABLE turn_lifecycle DROP CONSTRAINT turn_lifecycle_state_payload_shape;
    EXECUTE 'ALTER TABLE turn_lifecycle ADD CONSTRAINT turn_lifecycle_state_payload_shape CHECK (('
        || substr(definition, 8, length(definition) - 8) || ') OR (
            state_kind = ''active'' AND active_phase_kind = ''awaiting_credential_availability''
            AND start_lineage_kind IS NOT NULL AND starting_frontier_id IS NOT NULL
            AND current_attempt_id IS NULL AND terminal_frontier_id IS NULL
            AND terminal_disposition_kind IS NULL AND terminal_attempt_id IS NULL
            AND terminal_model_call_id IS NULL AND terminal_tool_attempt_id IS NULL
            AND recovery_model_call_id IS NULL AND approval_tool_request_id IS NULL
            AND recovery_tool_attempt_id IS NULL AND child_wait_request_id IS NULL
            AND runner_recovery_runner_id IS NULL AND runner_recovery_placement_revision IS NULL
            AND runner_recovery_tool_attempt_id IS NULL))';
END $$;

CREATE FUNCTION assert_credential_availability_wait(checked_turn uuid) RETURNS void LANGUAGE plpgsql AS $$
DECLARE waiting credential_availability_wait%ROWTYPE;
BEGIN
    SELECT * INTO waiting FROM credential_availability_wait
        WHERE turn_id = checked_turn AND consumed_by_attempt_id IS NULL;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'credential availability phase lacks its wait' USING ERRCODE = '23514';
    END IF;
    IF NOT EXISTS (SELECT 1 FROM turn_attempt attempt
        WHERE attempt.turn_attempt_id = waiting.wait_attempt_id
          AND attempt.turn_id = waiting.turn_id AND attempt.session_id = waiting.session_id
          AND attempt.state_kind = 'ended' AND attempt.end_variant = 'without_stop'
          AND attempt.end_disposition = 'yielded_to_durable_wait'
          AND NOT EXISTS (SELECT 1 FROM model_call WHERE turn_attempt_id = attempt.turn_attempt_id)) THEN
        RAISE EXCEPTION 'credential wait requires its call-free yielded attempt' USING ERRCODE = '23514';
    END IF;
    IF NOT EXISTS (SELECT 1 FROM session_credential_pool_policy policy
        WHERE policy.session_id = waiting.session_id AND policy.effective_target_id = waiting.effective_target_id
          AND policy.pool_policy_id = waiting.pool_policy_id) THEN
        RAISE EXCEPTION 'credential wait policy differs from session admission' USING ERRCODE = '23514';
    END IF;
    IF waiting.predecessor_model_call_id IS NOT NULL AND NOT EXISTS (
        SELECT 1 FROM credential_pool_availability_successor successor
        JOIN model_call predecessor ON predecessor.model_call_id = successor.predecessor_model_call_id
        WHERE successor.successor_turn_attempt_id = waiting.wait_attempt_id
          AND successor.predecessor_model_call_id = waiting.predecessor_model_call_id
          AND successor.non_acceptance_proven = waiting.predecessor_non_acceptance_proven
          AND predecessor.session_id = waiting.session_id AND predecessor.turn_id = waiting.turn_id
          AND predecessor.state_kind = 'terminal' AND predecessor.terminal_disposition_kind = 'known_failed'
    ) THEN
        RAISE EXCEPTION 'credential wait predecessor proof differs' USING ERRCODE = '23514';
    END IF;
    IF (SELECT count(*) FROM credential_availability_wait_member WHERE wait_attempt_id = waiting.wait_attempt_id)
        <> (SELECT count(*) FROM credential_pool_policy_member WHERE pool_policy_id = waiting.pool_policy_id)
        OR EXISTS (SELECT 1 FROM credential_availability_wait_member member
            JOIN credential_pool_policy_member policy USING (pool_policy_id, ordinal)
            WHERE member.wait_attempt_id = waiting.wait_attempt_id AND member.profile <> policy.profile) THEN
        RAISE EXCEPTION 'credential wait member evidence is incomplete' USING ERRCODE = '23514';
    END IF;
END;
$$;

ALTER FUNCTION assert_turn_lifecycle_final_state(uuid) RENAME TO assert_turn_lifecycle_before_credential_wait;
CREATE FUNCTION assert_turn_lifecycle_final_state(checked_turn_id uuid) RETURNS void LANGUAGE plpgsql AS $$
BEGIN
    IF EXISTS (SELECT 1 FROM turn_lifecycle WHERE turn_id = checked_turn_id
        AND state_kind = 'active' AND active_phase_kind = 'awaiting_credential_availability') THEN
        PERFORM assert_credential_availability_wait(checked_turn_id);
        RETURN;
    END IF;
    PERFORM assert_turn_lifecycle_before_credential_wait(checked_turn_id);
END;
$$;

ALTER FUNCTION assert_turn_attempt_final_state(uuid) RENAME TO assert_turn_attempt_before_credential_wait;
CREATE FUNCTION assert_turn_attempt_final_state(checked_attempt_id uuid) RETURNS void LANGUAGE plpgsql AS $$
BEGIN
    IF EXISTS (SELECT 1 FROM credential_availability_wait_release release
        JOIN credential_availability_wait waiting ON waiting.wait_attempt_id = release.wait_attempt_id
        JOIN turn_attempt attempt ON attempt.turn_attempt_id = release.turn_attempt_id
        WHERE release.turn_attempt_id = checked_attempt_id
          AND waiting.consumed_by_attempt_id = attempt.turn_attempt_id
          AND attempt.continued_from_attempt_id = waiting.wait_attempt_id
          AND attempt.session_id = waiting.session_id AND attempt.turn_id = waiting.turn_id
          AND (attempt.state_kind = 'ended' OR EXISTS (SELECT 1 FROM model_call WHERE turn_attempt_id = attempt.turn_attempt_id))) THEN
        RETURN;
    END IF;
    PERFORM assert_turn_attempt_before_credential_wait(checked_attempt_id);
END;
$$;

CREATE TABLE credential_availability_wait_failure (
    turn_attempt_id uuid PRIMARY KEY REFERENCES credential_availability_wait_release,
    predecessor_model_call_id uuid NOT NULL REFERENCES model_call
);
CREATE TRIGGER credential_availability_wait_failure_immutable BEFORE UPDATE OR DELETE ON credential_availability_wait_failure
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

ALTER FUNCTION assert_failed_terminal_execution_before_context_headroom(uuid) RENAME TO assert_failed_terminal_execution_before_credential_wait_release;
CREATE FUNCTION assert_failed_terminal_execution_before_context_headroom(checked_turn_id uuid) RETURNS void LANGUAGE plpgsql AS $$
BEGIN
    IF EXISTS (SELECT 1 FROM credential_availability_wait_failure failure
        JOIN credential_availability_wait_release release USING (turn_attempt_id)
        JOIN credential_availability_wait waiting USING (wait_attempt_id)
        JOIN turn_attempt attempt USING (turn_attempt_id)
        JOIN turn_lifecycle lifecycle ON lifecycle.turn_id = attempt.turn_id AND lifecycle.session_id = attempt.session_id
        JOIN model_call predecessor ON predecessor.model_call_id = failure.predecessor_model_call_id
        WHERE lifecycle.turn_id = checked_turn_id AND lifecycle.state_kind = 'terminal'
          AND lifecycle.terminal_disposition_kind = 'failed' AND lifecycle.terminal_cause_kind = 'model_call_failed'
          AND lifecycle.terminal_attempt_id = attempt.turn_attempt_id AND lifecycle.terminal_model_call_id IS NULL
          AND attempt.state_kind = 'ended' AND attempt.end_variant = 'without_stop' AND attempt.end_disposition = 'known_failure'
          AND attempt.continued_from_attempt_id = waiting.wait_attempt_id
          AND waiting.consumed_by_attempt_id = attempt.turn_attempt_id
          AND waiting.predecessor_model_call_id = predecessor.model_call_id
          AND predecessor.turn_id = lifecycle.turn_id AND predecessor.session_id = lifecycle.session_id
          AND predecessor.state_kind = 'terminal' AND predecessor.terminal_disposition_kind = 'known_failed'
          AND NOT EXISTS (SELECT 1 FROM model_call WHERE turn_attempt_id = attempt.turn_attempt_id)) THEN
        RETURN;
    END IF;
    PERFORM assert_failed_terminal_execution_before_credential_wait_release(checked_turn_id);
END;
$$;

CREATE FUNCTION guard_credential_availability_wait_change() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'DELETE' OR OLD.consumed_by_attempt_id IS NOT NULL THEN
        RAISE EXCEPTION 'consumed credential wait is immutable' USING ERRCODE = '23514';
    END IF;
    IF (NEW.wait_attempt_id, NEW.session_id, NEW.turn_id, NEW.frontier_id,
        NEW.pool_policy_id, NEW.effective_target_id, NEW.predecessor_model_call_id,
        NEW.predecessor_non_acceptance_proven) IS DISTINCT FROM
       (OLD.wait_attempt_id, OLD.session_id, OLD.turn_id, OLD.frontier_id,
        OLD.pool_policy_id, OLD.effective_target_id, OLD.predecessor_model_call_id,
        OLD.predecessor_non_acceptance_proven) THEN
        RAISE EXCEPTION 'credential wait origin is immutable' USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER credential_availability_wait_origin_immutable BEFORE UPDATE OR DELETE ON credential_availability_wait
    FOR EACH ROW EXECUTE FUNCTION guard_credential_availability_wait_change();

CREATE FUNCTION require_credential_availability_wait_final_state() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE checked_turn uuid;
BEGIN
    SELECT turn_id INTO checked_turn FROM credential_availability_wait
        WHERE wait_attempt_id = COALESCE(NEW.wait_attempt_id, OLD.wait_attempt_id)
          AND consumed_by_attempt_id IS NULL;
    IF FOUND THEN
        PERFORM assert_credential_availability_wait(checked_turn);
        IF NOT EXISTS (SELECT 1 FROM turn_lifecycle WHERE turn_id = checked_turn
            AND state_kind = 'active' AND active_phase_kind = 'awaiting_credential_availability') THEN
            RAISE EXCEPTION 'credential wait lacks its active phase' USING ERRCODE = '23514';
        END IF;
    END IF;
    RETURN NULL;
END;
$$;
CREATE CONSTRAINT TRIGGER credential_availability_wait_final_state AFTER INSERT OR UPDATE ON credential_availability_wait
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_credential_availability_wait_final_state();
CREATE CONSTRAINT TRIGGER credential_availability_wait_member_final_state AFTER INSERT OR UPDATE OR DELETE ON credential_availability_wait_member
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_credential_availability_wait_final_state();

CREATE FUNCTION guard_credential_availability_wait_member_change() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'DELETE' OR EXISTS (SELECT 1 FROM credential_availability_wait
        WHERE wait_attempt_id = OLD.wait_attempt_id AND consumed_by_attempt_id IS NOT NULL)
        OR (NEW.wait_attempt_id, NEW.pool_policy_id, NEW.ordinal, NEW.profile) IS DISTINCT FROM
           (OLD.wait_attempt_id, OLD.pool_policy_id, OLD.ordinal, OLD.profile) THEN
        RAISE EXCEPTION 'credential wait member origin is immutable' USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER credential_availability_wait_member_origin_immutable BEFORE UPDATE OR DELETE ON credential_availability_wait_member
    FOR EACH ROW EXECUTE FUNCTION guard_credential_availability_wait_member_change();
