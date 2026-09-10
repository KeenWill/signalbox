DO $$
DECLARE call_id uuid;
BEGIN
    FOR call_id IN
        SELECT model_call_id FROM model_call_credential_pool_policy WHERE pool_policy_id IS NULL
    LOOP
        PERFORM retain_call_credential_pool_policy(call_id);
    END LOOP;
END;
$$;

ALTER TABLE model_call_credential_pool_policy
    ALTER COLUMN pool_policy_id SET NOT NULL;

ALTER TABLE credential_pool_exhaustion_member
    ADD COLUMN operationally_unavailable boolean NOT NULL DEFAULT false,
    ADD COLUMN operational_availability_recheck_at timestamptz,
    ADD CHECK (operationally_unavailable = (operational_availability_recheck_at IS NOT NULL));

CREATE OR REPLACE FUNCTION credential_pool_exhaustion_reconstruct(e credential_pool_exhaustion_member)
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
    SELECT 1, 0, NULL::bigint, NULL::bigint, NULL::uuid,
           floor(extract(epoch FROM e.operational_availability_recheck_at) * 1000)::bigint,
           jsonb_build_object('kind', 'membership_exclusion', 'record_generation', NULL)
      FROM context WHERE e.operationally_unavailable
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
