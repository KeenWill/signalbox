ALTER TABLE credential_exclusion DROP CONSTRAINT credential_exclusion_check,
    ADD CONSTRAINT credential_exclusion_oauth_generation_check CHECK (
        oauth_generation IS NULL OR (oauth_generation > 0 AND origin IN ('codex_home', 'oauth_refresh')));

CREATE OR REPLACE VIEW credential_exclusion_state AS
SELECT exclusion.*,
       EXISTS (SELECT 1 FROM clear_credential_exclusion_command AS command
               WHERE command.outcome = 'cleared'
                 AND command.cleared_generation = exclusion.record_generation) AS cleared,
       NOT EXISTS (SELECT 1 FROM credential_exclusion AS newer
                   WHERE newer.kind = exclusion.kind AND newer.profile = exclusion.profile
                     AND newer.origin = exclusion.origin
                     AND newer.pool_policy_id IS NOT DISTINCT FROM exclusion.pool_policy_id
                     AND newer.session_id IS NOT DISTINCT FROM exclusion.session_id
                     AND newer.record_generation > exclusion.record_generation)
       AND (exclusion.oauth_generation IS NULL OR EXISTS (
           SELECT 1 FROM oauth_credential_authorization oauth_auth
             WHERE oauth_auth.profile = exclusion.profile
               AND oauth_auth.generation = exclusion.oauth_generation
               AND oauth_auth.quarantined
               AND ((exclusion.origin = 'codex_home' AND oauth_auth.quarantine_cause = 'credential_home')
                 OR (exclusion.origin = 'oauth_refresh' AND oauth_auth.quarantine_cause <> 'credential_home'))))
       AND (exclusion.action_id IS NULL OR action.consumed_turn_id IS NULL)
       AND NOT EXISTS (SELECT 1 FROM clear_credential_exclusion_command AS command
                       WHERE command.outcome = 'cleared'
                         AND command.cleared_generation = exclusion.record_generation) AS active
  FROM credential_exclusion AS exclusion
  LEFT JOIN credential_pool_member_action AS action ON action.action_id = exclusion.action_id;

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
    member_action_ids bigint[] NOT NULL,
    cleared_record_generations bigint[] NOT NULL,
    oauth_home_generation bigint,
    oauth_refresh_generation bigint,
    transient_observation_model_call_ids uuid[] NOT NULL,
    capacity_windows jsonb CHECK (jsonb_typeof(capacity_windows) = 'array'),
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
CREATE TRIGGER credential_pool_exhaustion_member_cannot_be_truncated BEFORE TRUNCATE ON credential_pool_exhaustion_member
    FOR EACH STATEMENT EXECUTE FUNCTION reject_outbox_table_truncate();
CREATE TRIGGER credential_pool_chain_exclusion_immutable BEFORE UPDATE OR DELETE ON credential_pool_chain_exclusion
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();
CREATE TRIGGER credential_pool_chain_exclusion_cannot_be_truncated BEFORE TRUNCATE ON credential_pool_chain_exclusion
    FOR EACH STATEMENT EXECUTE FUNCTION reject_outbox_table_truncate();

CREATE FUNCTION capture_credential_pool_exhaustion_reset_sources() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    SELECT oauth_auth.generation INTO NEW.oauth_home_generation
      FROM oauth_credential_authorization oauth_auth
      WHERE oauth_auth.profile = NEW.profile AND oauth_auth.quarantined
        AND oauth_auth.quarantine_cause = 'credential_home';
    SELECT oauth_auth.generation INTO NEW.oauth_refresh_generation
      FROM oauth_credential_authorization oauth_auth
      WHERE oauth_auth.profile = NEW.profile AND oauth_auth.quarantined
        AND oauth_auth.quarantine_cause <> 'credential_home';
    SELECT COALESCE(array_agg(x.record_generation ORDER BY x.record_generation), '{}'::bigint[])
      INTO NEW.cleared_record_generations
      FROM credential_exclusion x
      WHERE x.profile = NEW.profile AND x.record_generation <= NEW.generation_ceiling
        AND EXISTS (SELECT 1 FROM clear_credential_exclusion_command cleared
            WHERE cleared.cleared_generation = x.record_generation AND cleared.outcome = 'cleared');
    SELECT COALESCE(array_agg(a.action_id ORDER BY a.action_id), '{}'::bigint[])
      INTO NEW.member_action_ids
      FROM credential_pool_member_action a
      WHERE a.credential_reference = NEW.profile AND a.consumed_turn_id IS NULL;
    SELECT COALESCE(array_agg(x.observation_model_call_id ORDER BY x.observation_model_call_id), '{}'::uuid[])
      INTO NEW.transient_observation_model_call_ids
      FROM credential_pool_transient_exclusion x
      WHERE x.credential_reference = NEW.profile AND x.reset_at > NEW.observed_at;
    SELECT snapshot.windows INTO NEW.capacity_windows
      FROM credential_rate_limit_snapshot snapshot
      WHERE snapshot.credential_reference = NEW.profile;
    RETURN NEW;
END;
$$;
CREATE TRIGGER credential_pool_exhaustion_capture_reset_sources BEFORE INSERT ON credential_pool_exhaustion_member
    FOR EACH ROW EXECUTE FUNCTION capture_credential_pool_exhaustion_reset_sources();

CREATE FUNCTION credential_pool_exhaustion_reconstruct(e credential_pool_exhaustion_member)
RETURNS TABLE (evidence jsonb, action_id bigint) LANGUAGE sql STABLE AS $$
WITH context AS (
    SELECT attempt.session_id, attempt.turn_id, lifecycle.acceptance_position,
           policy.definition->>'name' AS pool_name,
           COALESCE(member.headroom_reserve_percent, (policy.definition->>'headroom_reserve_percent')::integer) AS reserve
      FROM turn_attempt attempt
      JOIN turn_lifecycle lifecycle ON lifecycle.turn_id = attempt.turn_id
      JOIN credential_pool_policy policy ON policy.pool_policy_id = e.pool_policy_id
      JOIN credential_pool_policy_member member ON member.pool_policy_id = e.pool_policy_id AND member.ordinal = e.ordinal
      WHERE attempt.turn_attempt_id = e.terminal_attempt_id AND member.profile = e.profile
), projected AS (
    SELECT x.* FROM credential_exclusion x
      WHERE x.profile = e.profile AND x.record_generation <= e.generation_ceiling
), active_projected AS (
    SELECT x.* FROM projected x
      WHERE NOT EXISTS (
          SELECT 1 FROM projected newer
            WHERE newer.record_generation > x.record_generation
              AND newer.kind = x.kind AND newer.origin = x.origin
              AND newer.pool_policy_id IS NOT DISTINCT FROM x.pool_policy_id
              AND newer.session_id IS NOT DISTINCT FROM x.session_id)
        AND NOT (x.record_generation = ANY(e.cleared_record_generations))
        AND (x.oauth_generation IS NULL
          OR (x.origin = 'codex_home' AND x.oauth_generation = e.oauth_home_generation)
          OR (x.origin = 'oauth_refresh' AND x.oauth_generation = e.oauth_refresh_generation))
), windows AS (
    SELECT (capacity_window->>'remaining_percent')::bigint AS remaining,
           (capacity_window->>'resets_at')::numeric AS reset_nanos
      FROM jsonb_array_elements(COALESCE(e.capacity_windows, '[]'::jsonb)) capacity_window
), capacity AS (
    SELECT min(remaining) AS observed,
           max(floor(reset_nanos / 1000000)::bigint) AS reset_ms
      FROM windows CROSS JOIN context
      WHERE remaining <= context.reserve
        AND reset_nanos > extract(epoch FROM e.observed_at) * 1000000000
      HAVING count(*) > 0
), candidates AS (
    SELECT 0 AS rank, 0 AS source_order, x.record_generation, NULL::bigint AS action_id,
           NULL::uuid AS correlation, NULL::bigint AS reset_ms,
           jsonb_build_object('kind', x.kind, 'record_generation', x.record_generation) AS exclusion
      FROM active_projected x
      WHERE x.kind = 'profile_quarantine' AND x.origin <> 'pool_trigger'
    UNION ALL
    SELECT CASE a.action_kind WHEN 'quarantine' THEN 0 WHEN 'avoid_new_sessions' THEN 1 ELSE 2 END,
           1, x.record_generation, a.action_id, NULL::uuid, NULL::bigint,
           jsonb_build_object('kind', CASE a.action_kind WHEN 'quarantine' THEN 'profile_quarantine'
               WHEN 'avoid_new_sessions' THEN 'membership_exclusion' ELSE 'session_displacement' END,
               'record_generation', x.record_generation)
      FROM credential_pool_member_action a CROSS JOIN context
      LEFT JOIN projected x ON x.action_id = a.action_id
      WHERE a.action_id = ANY(e.member_action_ids) AND a.credential_reference = e.profile
        AND ((x.record_generation IS NULL AND (a.pool_name = context.pool_name OR a.action_kind = 'quarantine'))
          OR (x.record_generation IN (SELECT record_generation FROM active_projected)
              AND (x.kind = 'profile_quarantine' OR x.pool_policy_id = e.pool_policy_id)))
        AND (a.action_kind <> 'switch_next_turn'
          OR (a.observed_session_id = context.session_id AND a.observed_turn_id <> context.turn_id))
        AND (a.action_kind <> 'avoid_new_sessions' OR NOT EXISTS (
            SELECT 1 FROM model_call call
            JOIN model_call_credential_pool_policy policy USING (model_call_id)
            JOIN turn_lifecycle lifecycle ON lifecycle.turn_id = call.turn_id
              WHERE call.session_id = context.session_id AND call.credential_reference = e.profile
                AND policy.pool_name = context.pool_name AND call.state_kind = 'terminal'
                AND call.terminal_disposition_kind = 'completed'
                AND lifecycle.acceptance_position <= context.acceptance_position))
    UNION ALL
    SELECT 3, 0, NULL::bigint, NULL::bigint, chain.predecessor_model_call_id, NULL::bigint,
           jsonb_build_object('kind', 'chain_exclusion', 'predecessor_model_call_id', chain.predecessor_model_call_id)
      FROM credential_pool_chain_exclusion chain CROSS JOIN context
      WHERE chain.session_id = context.session_id AND chain.turn_id = context.turn_id AND chain.credential_reference = e.profile
    UNION ALL
    SELECT 4, 0, NULL::bigint, NULL::bigint, transient.observation_model_call_id,
           floor(extract(epoch FROM transient.reset_at) * 1000)::bigint,
           jsonb_build_object('kind', 'transient_exclusion', 'observation_model_call_id', transient.observation_model_call_id)
      FROM credential_pool_transient_exclusion transient
      WHERE transient.observation_model_call_id = ANY(e.transient_observation_model_call_ids)
        AND transient.credential_reference = e.profile AND transient.reset_at > e.observed_at
    UNION ALL
    SELECT 5, 0, NULL::bigint, NULL::bigint, NULL::uuid, capacity.reset_ms,
           jsonb_build_object('kind', 'headroom_reserve', 'observed_headroom_percent', capacity.observed,
               'reserve_percent', context.reserve)
      FROM capacity CROSS JOIN context
)
SELECT jsonb_build_object('profile', e.profile, 'exclusion', selected.exclusion, 'reset_at_unix_ms',
           (SELECT CASE WHEN count(reset_ms) = count(*) THEN max(reset_ms) END FROM candidates)),
       selected.action_id
  FROM candidates selected
  LEFT JOIN credential_pool_transient_exclusion transient_order
    ON selected.rank = 4 AND transient_order.observation_model_call_id = selected.correlation
  ORDER BY selected.rank, selected.source_order, selected.record_generation DESC NULLS LAST,
           selected.action_id DESC NULLS LAST, transient_order.reset_at DESC NULLS LAST,
           selected.reset_ms DESC NULLS LAST, selected.correlation
  LIMIT 1;
$$;

CREATE FUNCTION credential_pool_exhaustion_evidence_valid(attempt uuid) RETURNS boolean LANGUAGE sql STABLE AS $$
SELECT EXISTS (
    SELECT 1 FROM credential_pool_terminal_exhaustion h
    JOIN credential_pool_policy policy USING (pool_policy_id)
    WHERE h.terminal_attempt_id = attempt AND h.terminal_model_call_id IS NULL
      AND h.pool_name = policy.definition->>'name'
      AND (SELECT count(*) FROM credential_pool_exhaustion_member WHERE terminal_attempt_id = attempt)
          = (SELECT count(*) FROM credential_pool_policy_member WHERE pool_policy_id = h.pool_policy_id)
      AND (SELECT count(DISTINCT (observed_at, generation_ceiling))
             FROM credential_pool_exhaustion_member WHERE terminal_attempt_id = attempt) = 1
      AND NOT EXISTS (
        SELECT 1 FROM credential_pool_exhaustion_member e
        LEFT JOIN credential_pool_policy_member m ON m.pool_policy_id = h.pool_policy_id AND m.ordinal = e.ordinal
        LEFT JOIN LATERAL credential_pool_exhaustion_reconstruct(e) reconstructed ON true
        WHERE e.terminal_attempt_id = attempt AND NOT COALESCE(
          e.pool_policy_id = h.pool_policy_id AND e.profile = m.profile
          AND e.evidence = reconstructed.evidence
          AND e.action_id IS NOT DISTINCT FROM reconstructed.action_id, false)
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
CREATE TRIGGER credential_pool_exhaustion_outbox_cannot_be_truncated BEFORE TRUNCATE ON credential_pool_exhaustion_outbox_event
    FOR EACH STATEMENT EXECUTE FUNCTION reject_outbox_table_truncate();

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
