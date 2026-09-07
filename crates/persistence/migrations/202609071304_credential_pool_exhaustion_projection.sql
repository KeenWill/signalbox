ALTER TABLE credential_pool_terminal_exhaustion
    ADD COLUMN pool_policy_id uuid REFERENCES credential_pool_policy,
    ADD CONSTRAINT credential_pool_exhaustion_policy_unique UNIQUE (terminal_attempt_id, pool_policy_id),
    ADD CONSTRAINT credential_pool_exhaustion_pre_call_policy CHECK (
        terminal_model_call_id IS NOT NULL OR pool_policy_id IS NOT NULL) NOT VALID;

CREATE TABLE credential_pool_exhaustion_member (
    terminal_attempt_id uuid NOT NULL,
    pool_policy_id uuid NOT NULL,
    ordinal integer NOT NULL CHECK (ordinal BETWEEN 0 AND 1023),
    profile text NOT NULL,
    evidence jsonb NOT NULL,
    observed_at timestamptz NOT NULL,
    generation_ceiling bigint NOT NULL CHECK (generation_ceiling >= 0),
    action_id bigint REFERENCES credential_pool_member_action,
    record_generation bigint GENERATED ALWAYS AS ((evidence->'exclusion'->>'record_generation')::bigint) STORED REFERENCES credential_exclusion,
    predecessor_model_call_id uuid GENERATED ALWAYS AS ((evidence->'exclusion'->>'predecessor_model_call_id')::uuid) STORED REFERENCES model_call,
    observation_model_call_id uuid GENERATED ALWAYS AS ((evidence->'exclusion'->>'observation_model_call_id')::uuid) STORED REFERENCES credential_pool_transient_exclusion,
    PRIMARY KEY (terminal_attempt_id, ordinal),
    FOREIGN KEY (terminal_attempt_id, pool_policy_id) REFERENCES credential_pool_terminal_exhaustion (terminal_attempt_id, pool_policy_id) DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (pool_policy_id, ordinal) REFERENCES credential_pool_policy_member (pool_policy_id, ordinal),
    CHECK (evidence->>'profile' = profile AND evidence ? 'reset_at_unix_ms'),
    CHECK (evidence->'exclusion'->>'kind' IN ('profile_quarantine', 'membership_exclusion', 'session_displacement', 'chain_exclusion', 'transient_exclusion', 'headroom_reserve'))
);
CREATE TRIGGER credential_pool_exhaustion_member_immutable BEFORE UPDATE OR DELETE ON credential_pool_exhaustion_member
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE FUNCTION credential_pool_exhaustion_evidence_valid(attempt uuid) RETURNS boolean LANGUAGE sql STABLE AS $$
SELECT EXISTS (
    SELECT 1 FROM credential_pool_terminal_exhaustion h
    JOIN credential_pool_policy policy USING (pool_policy_id)
    WHERE h.terminal_attempt_id = attempt AND h.terminal_model_call_id IS NULL
      AND h.pool_name = policy.definition->>'name'
      AND (SELECT count(*) FROM credential_pool_exhaustion_member WHERE terminal_attempt_id = attempt)
          = (SELECT count(*) FROM credential_pool_policy_member WHERE pool_policy_id = h.pool_policy_id)
      AND EXISTS (SELECT 1 FROM credential_pool_exhaustion_member WHERE terminal_attempt_id = attempt)
      AND NOT EXISTS (
        SELECT 1 FROM credential_pool_exhaustion_member e
        LEFT JOIN credential_pool_policy_member m ON m.pool_policy_id = h.pool_policy_id AND m.ordinal = e.ordinal
        WHERE e.terminal_attempt_id = attempt AND NOT COALESCE(
          e.pool_policy_id = h.pool_policy_id AND e.profile = m.profile AND e.evidence->>'profile' = e.profile
          AND CASE e.evidence->'exclusion'->>'kind'
            WHEN 'profile_quarantine' THEN e.evidence->'reset_at_unix_ms' = 'null'::jsonb
            WHEN 'membership_exclusion' THEN e.evidence->'reset_at_unix_ms' = 'null'::jsonb
            WHEN 'session_displacement' THEN e.evidence->'reset_at_unix_ms' = 'null'::jsonb
            WHEN 'chain_exclusion' THEN e.evidence->'reset_at_unix_ms' = 'null'::jsonb
            ELSE (e.evidence->>'reset_at_unix_ms')::bigint >= floor(extract(epoch FROM e.observed_at) * 1000)::bigint
          END
          AND CASE
          WHEN e.evidence->'exclusion'->>'kind' IN ('profile_quarantine', 'membership_exclusion', 'session_displacement') THEN
            CASE WHEN e.record_generation IS NOT NULL THEN EXISTS (
                SELECT 1 FROM credential_exclusion x WHERE x.record_generation = e.record_generation
                  AND x.record_generation <= e.generation_ceiling AND x.profile = e.profile
                  AND x.kind = e.evidence->'exclusion'->>'kind'
                  AND (x.kind = 'profile_quarantine' OR x.pool_policy_id = h.pool_policy_id)
                  AND (x.kind <> 'session_displacement' OR x.session_id = h.session_id)
                  AND NOT EXISTS (SELECT 1 FROM credential_exclusion newer
                      WHERE newer.record_generation > x.record_generation AND newer.record_generation <= e.generation_ceiling
                        AND newer.profile = x.profile AND newer.kind = x.kind AND newer.origin = x.origin
                        AND newer.pool_policy_id IS NOT DISTINCT FROM x.pool_policy_id
                        AND newer.session_id IS NOT DISTINCT FROM x.session_id)
            ) ELSE e.evidence->'exclusion'->'record_generation' = 'null'::jsonb AND EXISTS (
                SELECT 1 FROM credential_pool_member_action a WHERE a.action_id = e.action_id
                  AND a.credential_reference = e.profile
                  AND (a.pool_name = h.pool_name OR a.action_kind = 'quarantine')
                  AND CASE a.action_kind WHEN 'quarantine' THEN 'profile_quarantine' WHEN 'avoid_new_sessions' THEN 'membership_exclusion' ELSE 'session_displacement' END = e.evidence->'exclusion'->>'kind'
                  AND (a.action_kind <> 'switch_next_turn' OR (a.observed_session_id = h.session_id AND a.observed_turn_id <> h.turn_id))
                  AND NOT EXISTS (SELECT 1 FROM credential_exclusion x WHERE x.action_id = a.action_id AND x.record_generation <= e.generation_ceiling)
            ) END
          WHEN e.evidence->'exclusion'->>'kind' = 'chain_exclusion' THEN EXISTS (
            SELECT 1 FROM credential_pool_chain_exclusion x WHERE x.session_id = h.session_id AND x.turn_id = h.turn_id AND x.credential_reference = e.profile AND x.predecessor_model_call_id = e.predecessor_model_call_id)
          WHEN e.evidence->'exclusion'->>'kind' = 'transient_exclusion' THEN EXISTS (
            SELECT 1 FROM credential_pool_transient_exclusion x WHERE x.observation_model_call_id = e.observation_model_call_id AND x.credential_reference = e.profile AND x.reset_at > e.observed_at
              AND floor(extract(epoch FROM x.reset_at) * 1000)::bigint <= (e.evidence->>'reset_at_unix_ms')::bigint)
          WHEN e.evidence->'exclusion'->>'kind' = 'headroom_reserve' THEN
            (e.evidence->'exclusion'->>'reserve_percent')::integer = COALESCE(m.headroom_reserve_percent, (policy.definition->>'headroom_reserve_percent')::integer)
            AND (e.evidence->'exclusion'->>'observed_headroom_percent')::bigint <= (e.evidence->'exclusion'->>'reserve_percent')::integer
          ELSE false END, false)
      )
);
$$;

CREATE FUNCTION require_credential_pool_exhaustion_evidence() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.terminal_model_call_id IS NULL AND NOT credential_pool_exhaustion_evidence_valid(NEW.terminal_attempt_id) THEN
        RAISE EXCEPTION 'invalid credential pool exhaustion evidence' USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;
CREATE CONSTRAINT TRIGGER credential_pool_exhaustion_requires_evidence AFTER INSERT ON credential_pool_terminal_exhaustion
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_credential_pool_exhaustion_evidence();

DO $$ DECLARE definition text; BEGIN
    SELECT pg_get_constraintdef(oid) INTO definition FROM pg_constraint WHERE conrelid = 'outbox_event'::regclass AND conname = 'outbox_event_kind_closed';
    ALTER TABLE outbox_event DROP CONSTRAINT outbox_event_kind_closed;
    EXECUTE 'ALTER TABLE outbox_event ADD CONSTRAINT outbox_event_kind_closed CHECK ((' || substr(definition, 8, length(definition) - 8) || ') OR event_kind = ''turn_credential_pool_exhausted'')';
END $$;
CREATE TABLE credential_pool_exhaustion_outbox_event (
    event_sequence numeric(20,0) PRIMARY KEY,
    event_kind text NOT NULL CHECK (event_kind = 'turn_credential_pool_exhausted'),
    storage_version smallint NOT NULL CHECK (storage_version = 1),
    session_id uuid NOT NULL,
    terminal_attempt_id uuid NOT NULL UNIQUE REFERENCES credential_pool_terminal_exhaustion,
    FOREIGN KEY (event_sequence, event_kind, storage_version, session_id) REFERENCES outbox_event(event_sequence, event_kind, storage_version, session_id) DEFERRABLE INITIALLY DEFERRED
);
CREATE TRIGGER credential_pool_exhaustion_outbox_immutable BEFORE UPDATE OR DELETE ON credential_pool_exhaustion_outbox_event
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE OR REPLACE FUNCTION require_outbox_event_typed_record() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    matching_records bigint;
BEGIN
    CASE NEW.event_kind
        WHEN 'session_created' THEN
            SELECT count(*) INTO matching_records
              FROM session_created_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'session_state_changed' THEN
            SELECT count(*) INTO matching_records
              FROM session_state_changed_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'session_terminal' THEN
            SELECT count(*) INTO matching_records
              FROM session_terminal_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'turn_credential_pool_exhausted' THEN
            SELECT count(*) INTO matching_records FROM credential_pool_exhaustion_outbox_event WHERE event_sequence = NEW.event_sequence;
        WHEN 'turn_terminal' THEN
            SELECT count(*) INTO matching_records
              FROM turn_terminal_outbox_event
             WHERE event_sequence = NEW.event_sequence
               AND disposition_kind = NEW.turn_disposition;
        WHEN 'goal_changed' THEN
            SELECT count(*) INTO matching_records
              FROM goal_changed_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'command_settled' THEN
            SELECT count(*) INTO matching_records
              FROM command_settled_outbox_event
             WHERE event_sequence = NEW.event_sequence
               AND session_id IS NOT DISTINCT FROM NEW.session_id;
        WHEN 'injection_settled' THEN
            SELECT count(*) INTO matching_records
              FROM injection_settled_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'session_ownership_changed' THEN
            SELECT count(*) INTO matching_records
              FROM session_ownership_changed_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'session_model_settings_changed' THEN
            SELECT count(*) INTO matching_records
              FROM session_model_settings_changed_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'turn_model_settings_resolved' THEN
            SELECT count(*) INTO matching_records
              FROM turn_model_settings_resolved_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'input_accepted' THEN
            SELECT count(*) INTO matching_records
              FROM input_accepted_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'turn_activated' THEN
            SELECT count(*) INTO matching_records
              FROM turn_activated_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'model_call_transition' THEN
            SELECT count(*) INTO matching_records
              FROM model_call_transition_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'tool_batch_transition' THEN
            SELECT count(*) INTO matching_records
              FROM tool_batch_transition_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'tool_approval_decided' THEN
            SELECT count(*) INTO matching_records
              FROM tool_approval_decided_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'context_compacted' THEN
            SELECT count(*) INTO matching_records
              FROM context_compacted_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'runner_state_transition' THEN
            SELECT count(*) INTO matching_records
              FROM runner_state_transition_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        ELSE
            RAISE EXCEPTION 'unsupported outbox event kind %', NEW.event_kind
                USING ERRCODE = '23514';
    END CASE;

    IF matching_records <> 1 THEN
        RAISE EXCEPTION 'outbox event % requires exactly one % typed record',
            NEW.event_sequence,
            NEW.event_kind
            USING ERRCODE = '23503';
    END IF;
    RETURN NULL;
END;
$$;

CREATE OR REPLACE FUNCTION outbox_event_timeline_kind(kind text, disposition text) RETURNS text
    LANGUAGE sql IMMUTABLE
    AS $$
    SELECT CASE
        WHEN kind = 'turn_credential_pool_exhausted' THEN 'turn_failed'
        WHEN kind <> 'turn_terminal' THEN kind
        WHEN disposition = 'retired' THEN 'goal_turn_retired'
        ELSE 'turn_' || disposition
    END;
$$;

CREATE OR REPLACE FUNCTION record_operator_attention_outbox_change() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NEW.event_kind IN (
        'session_state_changed', 'session_terminal', 'session_ownership_changed',
        'goal_changed', 'command_settled', 'injection_settled',
        'turn_credential_pool_exhausted'
    ) THEN
        RETURN NULL;
    END IF;
    INSERT INTO operator_attention_change (session_id, fact_kind)
    VALUES (
        NEW.session_id,
        CASE NEW.event_kind
            WHEN 'session_created' THEN 'session'
            WHEN 'session_model_settings_changed' THEN 'session'
            WHEN 'turn_terminal' THEN CASE NEW.turn_disposition
                WHEN 'retired' THEN 'goal'
                ELSE 'turn'
            END
            WHEN 'runner_state_transition' THEN 'runner'
            WHEN 'turn_model_settings_resolved' THEN 'turn'
            WHEN 'input_accepted' THEN 'turn'
            WHEN 'turn_activated' THEN 'turn'
            WHEN 'model_call_transition' THEN 'turn'
            WHEN 'tool_batch_transition' THEN 'turn'
            WHEN 'tool_approval_decided' THEN 'turn'
            WHEN 'context_compacted' THEN 'turn'
            ELSE NULL
        END
    );
    RETURN NULL;
END;
$$;

CREATE OR REPLACE FUNCTION assert_failed_terminal_execution_before_context_headroom(checked_turn_id uuid) RETURNS void
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF EXISTS (
        SELECT 1
          FROM credential_pool_terminal_exhaustion AS exhausted
          JOIN turn_lifecycle AS lifecycle
            ON lifecycle.turn_id = exhausted.turn_id
           AND lifecycle.session_id = exhausted.session_id
          JOIN turn_attempt AS attempt
            ON attempt.turn_attempt_id = exhausted.terminal_attempt_id
           AND attempt.turn_id = exhausted.turn_id
           AND attempt.session_id = exhausted.session_id
          LEFT JOIN model_call AS call
            ON call.model_call_id = exhausted.terminal_model_call_id
           AND call.turn_attempt_id = exhausted.terminal_attempt_id
           AND call.turn_id = exhausted.turn_id
           AND call.session_id = exhausted.session_id
         WHERE exhausted.turn_id = checked_turn_id
           AND lifecycle.state_kind = 'terminal'
           AND lifecycle.terminal_disposition_kind = 'failed'
           AND lifecycle.terminal_attempt_id = exhausted.terminal_attempt_id
           AND lifecycle.terminal_model_call_id IS NOT DISTINCT FROM
               exhausted.terminal_model_call_id
           AND attempt.state_kind = 'ended'
           AND attempt.end_disposition = 'known_failure'
           AND (
                exhausted.terminal_model_call_id IS NULL
                OR (
                    call.state_kind = 'terminal'
                    AND call.terminal_disposition_kind = 'known_failed'
                )
           )
    ) OR EXISTS (
        SELECT 1 FROM turn_lifecycle lifecycle
        JOIN turn_attempt attempt ON attempt.turn_attempt_id = lifecycle.terminal_attempt_id
            AND attempt.session_id = lifecycle.session_id AND attempt.turn_id = lifecycle.turn_id
        JOIN credential_pool_availability_successor successor
            ON successor.successor_turn_attempt_id = attempt.turn_attempt_id
        JOIN model_call predecessor ON predecessor.model_call_id = successor.predecessor_model_call_id
            AND predecessor.session_id = lifecycle.session_id AND predecessor.turn_id = lifecycle.turn_id
        WHERE lifecycle.turn_id = checked_turn_id AND lifecycle.state_kind = 'terminal'
            AND lifecycle.terminal_disposition_kind = 'failed'
            AND lifecycle.terminal_cause_kind = 'model_call_failed'
            AND lifecycle.terminal_model_call_id IS NULL
            AND attempt.state_kind = 'ended' AND attempt.end_variant = 'without_stop'
            AND attempt.end_disposition = 'known_failure'
            AND predecessor.state_kind = 'terminal' AND predecessor.terminal_disposition_kind = 'known_failed'
            AND NOT EXISTS (SELECT 1 FROM model_call WHERE turn_attempt_id = attempt.turn_attempt_id)
            AND NOT EXISTS (SELECT 1 FROM credential_pool_chain_exclusion
                WHERE predecessor_model_call_id = predecessor.model_call_id)
    ) THEN
        RETURN;
    END IF;

    PERFORM assert_failed_terminal_execution_before_credential_pools(
        checked_turn_id
    );
END;
$$;
