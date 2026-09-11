ALTER TABLE credential_pool_terminal_exhaustion
    DROP CONSTRAINT credential_pool_terminal_exhaustion_cause_kind_check,
    ADD CONSTRAINT credential_pool_terminal_exhaustion_cause_kind_check CHECK (
        cause_kind IN ('rate_limited', 'quota_exhausted', 'overloaded',
                       'credential_rejected', 'provider_internal'));

ALTER TABLE credential_pool_chain_exclusion
    DROP CONSTRAINT credential_pool_chain_exclusion_cause_kind_check,
    ADD CONSTRAINT credential_pool_chain_exclusion_cause_kind_check CHECK (
        cause_kind IN ('rate_limited', 'quota_exhausted', 'overloaded',
                       'credential_rejected', 'provider_internal')),
    ADD COLUMN observed_at timestamptz NOT NULL DEFAULT clock_timestamp();

ALTER TABLE credential_authentication_release RENAME TO credential_pool_exclusion_release;
ALTER TABLE credential_pool_exclusion_release
    ALTER COLUMN command_id DROP NOT NULL,
    ADD COLUMN capacity_observed_at_nanos numeric(29, 0),
    ADD CONSTRAINT credential_pool_exclusion_release_evidence CHECK (
        (command_id IS NOT NULL) <> (capacity_observed_at_nanos IS NOT NULL));
ALTER TRIGGER credential_authentication_release_immutable ON credential_pool_exclusion_release
    RENAME TO credential_pool_exclusion_release_immutable;
ALTER TRIGGER credential_authentication_release_cannot_be_truncated ON credential_pool_exclusion_release
    RENAME TO credential_pool_exclusion_release_cannot_be_truncated;

ALTER TABLE credential_availability_wait
    DROP CONSTRAINT credential_availability_wait_cause_check,
    ADD CONSTRAINT credential_availability_wait_cause_check
        CHECK (cause IN ('contended', 'exhausted', 'network_unavailable'));

DO $migration$
DECLARE definition text;
BEGIN
    SELECT pg_get_functiondef('credential_pool_exhaustion_reconstruct(credential_pool_exhaustion_member)'::regprocedure)
      INTO definition;
    EXECUTE replace(definition, 'credential_authentication_release', 'credential_pool_exclusion_release');
END;
$migration$;

CREATE FUNCTION release_network_credential_waits() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO credential_pool_exclusion_release
        (predecessor_model_call_id, capacity_observed_at_nanos)
    SELECT chain.predecessor_model_call_id, NEW.observed_at_nanos
      FROM credential_pool_chain_exclusion chain
      JOIN credential_availability_wait waiting USING (session_id, turn_id)
     WHERE chain.credential_reference = NEW.credential_reference
       AND chain.cause_kind = 'provider_internal'
       AND waiting.consumed_by_attempt_id IS NULL
       AND NEW.observed_at_nanos > extract(epoch FROM chain.observed_at) * 1000000000
       AND jsonb_array_length(NEW.windows) > 0
    ON CONFLICT DO NOTHING;
    PERFORM wake_credential_member(NEW.credential_reference);
    RETURN NULL;
END;
$$;

DROP TRIGGER credential_wait_capacity_update ON credential_rate_limit_snapshot;
CREATE TRIGGER credential_wait_capacity_update AFTER INSERT OR UPDATE ON credential_rate_limit_snapshot
    FOR EACH ROW EXECUTE FUNCTION release_network_credential_waits();
