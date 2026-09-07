CREATE TABLE credential_pool_policy (
    pool_policy_id uuid PRIMARY KEY,
    definition jsonb NOT NULL CHECK (jsonb_typeof(definition) = 'object')
);
CREATE TABLE credential_pool_policy_member (
    pool_policy_id uuid NOT NULL REFERENCES credential_pool_policy,
    ordinal integer NOT NULL CHECK (ordinal BETWEEN 0 AND 1023),
    profile text NOT NULL CHECK (octet_length(profile) BETWEEN 1 AND 256),
    priority bigint NOT NULL CHECK (priority BETWEEN 1 AND 4294967295),
    headroom_reserve_percent smallint CHECK (headroom_reserve_percent BETWEEN 0 AND 100),
    PRIMARY KEY (pool_policy_id, ordinal), UNIQUE (pool_policy_id, profile)
);
CREATE FUNCTION retain_credential_pool_policy(candidate uuid, policy jsonb) RETURNS uuid LANGUAGE plpgsql AS $$
DECLARE retained uuid;
BEGIN
    PERFORM pg_advisory_xact_lock(hashtextextended('credential_pool_policy:' || (policy->>'name'), 0));
    SELECT pool_policy_id INTO retained FROM credential_pool_policy WHERE definition = policy;
    IF retained IS NULL THEN
        retained := candidate;
        INSERT INTO credential_pool_policy VALUES (retained, policy);
        INSERT INTO credential_pool_policy_member
        SELECT retained, ordinal::integer - 1, member->>'profile',
               (member->>'priority')::bigint, (member->>'headroom_reserve_percent')::smallint
          FROM jsonb_array_elements(policy->'members') WITH ORDINALITY AS members(member, ordinal);
    END IF;
    RETURN retained;
END;
$$;
CREATE TRIGGER credential_pool_policy_immutable BEFORE UPDATE OR DELETE ON credential_pool_policy
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();
CREATE TRIGGER credential_pool_policy_member_immutable BEFORE UPDATE OR DELETE ON credential_pool_policy_member
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();
ALTER TABLE model_call_credential_pool_policy ADD COLUMN pool_policy_id uuid REFERENCES credential_pool_policy;

CREATE FUNCTION retain_call_credential_pool_policy(call_id uuid) RETURNS uuid LANGUAGE plpgsql AS $$
DECLARE retained uuid; policy jsonb;
BEGIN
    SELECT pool_policy_id INTO retained FROM model_call_credential_pool_policy WHERE model_call_id = call_id;
    IF retained IS NOT NULL THEN RETURN retained; END IF;
    SELECT jsonb_build_object(
        'name', pool_name, 'on_pool_exhausted', on_pool_exhausted,
        'on_quota_exhausted', on_quota_exhausted, 'on_rate_limited', on_rate_limited,
        'on_overloaded', on_overloaded, 'on_credential_rejected', on_credential_rejected,
        'tie_break', tie_break, 'headroom_reserve_percent', headroom_reserve_percent,
        'on_headroom_low', on_headroom_low,
        'members', (SELECT jsonb_agg(jsonb_build_object('profile', credential_reference,
            'priority', priority, 'headroom_reserve_percent', headroom_reserve_percent) ORDER BY member_ordinal)
            FROM model_call_credential_pool_member WHERE model_call_id = call_id))
      INTO policy FROM model_call_credential_pool_policy WHERE model_call_id = call_id;
    IF policy IS NULL OR policy->'members' = 'null'::jsonb THEN
        RAISE EXCEPTION 'credential action has no complete policy' USING ERRCODE = '23514';
    END IF;
    retained := retain_credential_pool_policy(call_id, policy);
    UPDATE model_call_credential_pool_policy SET pool_policy_id = retained WHERE model_call_id = call_id;
    RETURN retained;
END;
$$;

CREATE TABLE credential_exclusion (
    record_generation bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    kind text NOT NULL CHECK (kind IN ('profile_quarantine', 'membership_exclusion', 'session_displacement')),
    profile text NOT NULL CHECK (octet_length(profile) BETWEEN 1 AND 256),
    pool_policy_id uuid REFERENCES credential_pool_policy,
    session_id uuid REFERENCES session,
    origin text NOT NULL CHECK (origin IN ('pool_trigger', 'codex_home', 'oauth_refresh')),
    action_id bigint UNIQUE REFERENCES credential_pool_member_action,
    CHECK ((kind = 'profile_quarantine' AND pool_policy_id IS NULL AND session_id IS NULL)
        OR (kind = 'membership_exclusion' AND pool_policy_id IS NOT NULL AND session_id IS NULL AND origin = 'pool_trigger')
        OR (kind = 'session_displacement' AND pool_policy_id IS NOT NULL AND session_id IS NOT NULL AND origin = 'pool_trigger')),
    CHECK ((origin = 'pool_trigger') = (action_id IS NOT NULL))
);
CREATE TRIGGER credential_exclusion_immutable BEFORE UPDATE OR DELETE ON credential_exclusion
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();
CREATE FUNCTION record_credential_action_exclusion() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE policy_id uuid;
BEGIN
    policy_id := retain_call_credential_pool_policy(NEW.observation_model_call_id);
    INSERT INTO credential_exclusion (kind, profile, pool_policy_id, session_id, origin, action_id)
    VALUES (CASE NEW.action_kind WHEN 'quarantine' THEN 'profile_quarantine'
                WHEN 'avoid_new_sessions' THEN 'membership_exclusion' ELSE 'session_displacement' END,
            NEW.credential_reference, CASE WHEN NEW.action_kind <> 'quarantine' THEN policy_id END,
            CASE WHEN NEW.action_kind = 'switch_next_turn' THEN NEW.observed_session_id END,
            'pool_trigger', NEW.action_id);
    RETURN NULL;
END;
$$;
CREATE TRIGGER credential_action_exclusion AFTER INSERT ON credential_pool_member_action
    FOR EACH ROW EXECUTE FUNCTION record_credential_action_exclusion();

ALTER TABLE durable_command DROP CONSTRAINT durable_command_kind_closed;
ALTER TABLE durable_command ADD CONSTRAINT durable_command_kind_closed CHECK ((command_kind = ANY (ARRAY['create_session'::text, 'create_session_from_imported_frontier'::text, 'replace_session_defaults'::text, 'replace_session_metadata'::text, 'submit_input'::text, 'decide_tool_request'::text, 'override_denied_tool_request'::text, 'review_workflow'::text, 'review_orchestration'::text, 'compact_session'::text, 'goal'::text, 'update_session_placement'::text, 'register_workspace'::text, 'mint_git_remote'::text, 'withdraw_git_remote'::text, 'session_lifecycle'::text, 'clear_credential_exclusion'::text, 'provision_oauth_credential'::text, 'reprovision_oauth_credential'::text, 'delete_oauth_credential'::text])));

DO $$
DECLARE item record;
BEGIN
 FOR item IN SELECT conname, pg_get_expr(conbin, conrelid) AS expression FROM pg_constraint
   WHERE conrelid = 'durable_command'::regclass AND conname = 'durable_command_storage_version_supported' LOOP
   EXECUTE format('ALTER TABLE durable_command DROP CONSTRAINT %I', item.conname);
   EXECUTE format('ALTER TABLE durable_command ADD CONSTRAINT %I CHECK ((%s) OR (command_kind = %L AND storage_version = 1))', item.conname, item.expression, 'clear_credential_exclusion');
 END LOOP;
END;
$$;
CREATE TABLE clear_credential_exclusion_command (
    command_id uuid PRIMARY KEY,
    command_kind text NOT NULL DEFAULT 'clear_credential_exclusion' CHECK (command_kind = 'clear_credential_exclusion'),
    storage_version smallint NOT NULL DEFAULT 1 CHECK (storage_version = 1),
    target jsonb NOT NULL CHECK (jsonb_typeof(target) = 'object'),
    outcome text NOT NULL CHECK (outcome IN ('cleared', 'already_cleared', 'stale_generation', 'unknown_credential_exclusion')),
    cleared_generation bigint GENERATED ALWAYS AS (
        CASE WHEN outcome IN ('cleared', 'already_cleared') THEN (target->>'record_generation')::bigint END
    ) STORED REFERENCES credential_exclusion(record_generation),
    FOREIGN KEY (command_id, command_kind, storage_version) REFERENCES durable_command (command_id, command_kind, storage_version) DEFERRABLE INITIALLY DEFERRED
);
CREATE TRIGGER clear_credential_exclusion_command_immutable BEFORE UPDATE OR DELETE ON clear_credential_exclusion_command
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();
CREATE FUNCTION authenticate_cleared_credential_exclusion() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE source credential_exclusion; expected jsonb;
BEGIN
    IF NEW.cleared_generation IS NULL THEN RETURN NULL; END IF;
    SELECT * INTO STRICT source FROM credential_exclusion WHERE record_generation = NEW.cleared_generation;
    expected := jsonb_build_object('kind', source.kind, 'profile', source.profile,
                                  'record_generation', source.record_generation);
    IF source.pool_policy_id IS NOT NULL THEN
        expected := expected || jsonb_build_object('pool_policy_id', source.pool_policy_id);
    END IF;
    IF source.session_id IS NOT NULL THEN
        expected := expected || jsonb_build_object('session_id', source.session_id);
    END IF;
    IF NEW.target <> expected OR source.origin = 'oauth_refresh' THEN
        RAISE EXCEPTION 'clear target differs from its clearable source' USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;
CREATE CONSTRAINT TRIGGER clear_credential_exclusion_source
    AFTER INSERT ON clear_credential_exclusion_command DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION authenticate_cleared_credential_exclusion();
CREATE OR REPLACE FUNCTION require_durable_command_typed_record() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE matching_records bigint;
BEGIN
    IF NEW.command_kind <> 'review_orchestration' AND EXISTS (
        SELECT 1 FROM review_orchestration_command_recovery
         WHERE command_id = NEW.command_id
    ) THEN
        RAISE EXCEPTION 'durable command % is reserved by review orchestration recovery', NEW.command_id
            USING ERRCODE = '23505';
    END IF;
    CASE NEW.command_kind
        WHEN 'create_session' THEN SELECT count(*) INTO matching_records FROM create_session_command WHERE command_id = NEW.command_id;
        WHEN 'create_session_from_imported_frontier' THEN SELECT count(*) INTO matching_records FROM create_session_from_imported_frontier_command WHERE command_id = NEW.command_id;
        WHEN 'replace_session_defaults' THEN SELECT count(*) INTO matching_records FROM replace_session_defaults_command WHERE command_id = NEW.command_id;
        WHEN 'replace_session_metadata' THEN SELECT count(*) INTO matching_records FROM replace_session_metadata_command WHERE command_id = NEW.command_id;
        WHEN 'submit_input' THEN SELECT count(*) INTO matching_records FROM submit_input_command WHERE command_id = NEW.command_id;
        WHEN 'decide_tool_request' THEN SELECT count(*) INTO matching_records FROM decide_tool_request_command WHERE command_id = NEW.command_id;
        WHEN 'override_denied_tool_request' THEN SELECT count(*) INTO matching_records FROM override_denied_tool_request_command WHERE command_id = NEW.command_id;
        WHEN 'review_workflow' THEN SELECT count(*) INTO matching_records FROM review_workflow_command WHERE command_id = NEW.command_id;
        WHEN 'review_orchestration' THEN SELECT (SELECT count(*) FROM review_orchestration_command WHERE command_id = NEW.command_id) + (SELECT count(*) FROM review_orchestration_command_intent WHERE command_id = NEW.command_id) INTO matching_records;
        WHEN 'compact_session' THEN SELECT count(*) INTO matching_records FROM compact_session_command WHERE command_id = NEW.command_id;
        WHEN 'goal' THEN SELECT count(*) INTO matching_records FROM goal_command WHERE command_id = NEW.command_id;
        WHEN 'update_session_placement' THEN SELECT count(*) INTO matching_records FROM update_session_placement_command WHERE command_id = NEW.command_id;
        WHEN 'register_workspace' THEN SELECT count(*) INTO matching_records FROM workspace WHERE command_id = NEW.command_id;
        WHEN 'mint_git_remote' THEN SELECT count(*) INTO matching_records FROM configured_git_remote_mint WHERE command_id = NEW.command_id;
        WHEN 'withdraw_git_remote' THEN SELECT count(*) INTO matching_records FROM configured_git_remote_withdrawal WHERE command_id = NEW.command_id;
        WHEN 'session_lifecycle' THEN SELECT count(*) INTO matching_records FROM session_lifecycle_command WHERE command_id = NEW.command_id;
        WHEN 'provision_oauth_credential' THEN SELECT count(*) INTO matching_records FROM provision_oauth_credential_command WHERE command_id = NEW.command_id;
        WHEN 'reprovision_oauth_credential' THEN SELECT count(*) INTO matching_records FROM reprovision_oauth_credential_command WHERE command_id = NEW.command_id;
        WHEN 'delete_oauth_credential' THEN SELECT count(*) INTO matching_records FROM delete_oauth_credential_command WHERE command_id = NEW.command_id;
        WHEN 'clear_credential_exclusion' THEN SELECT count(*) INTO matching_records FROM clear_credential_exclusion_command WHERE command_id = NEW.command_id;
        ELSE RAISE EXCEPTION 'unsupported durable command kind %', NEW.command_kind USING ERRCODE = '23514';
    END CASE;
    IF matching_records <> 1 THEN
        RAISE EXCEPTION 'durable command % requires exactly one % typed record', NEW.command_id, NEW.command_kind USING ERRCODE = '23503';
    END IF;
    RETURN NULL;
END;
$$;
CREATE VIEW credential_exclusion_state AS
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
       AND (exclusion.action_id IS NULL OR action.consumed_turn_id IS NULL)
       AND NOT EXISTS (SELECT 1 FROM clear_credential_exclusion_command AS command
                       WHERE command.outcome = 'cleared'
                         AND command.cleared_generation = exclusion.record_generation) AS active
  FROM credential_exclusion AS exclusion
  LEFT JOIN credential_pool_member_action AS action ON action.action_id = exclusion.action_id;
