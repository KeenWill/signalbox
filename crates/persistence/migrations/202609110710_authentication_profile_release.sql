ALTER TABLE credential_pool_chain_exclusion
    DROP CONSTRAINT credential_pool_chain_exclusion_pkey,
    ADD PRIMARY KEY (session_id, turn_id, credential_reference, predecessor_model_call_id);

CREATE TABLE credential_authentication_release (
    predecessor_model_call_id uuid PRIMARY KEY
        REFERENCES credential_pool_chain_exclusion(predecessor_model_call_id),
    command_id uuid NOT NULL REFERENCES reload_configuration_command(command_id),
    released_at timestamptz NOT NULL DEFAULT clock_timestamp()
);

CREATE TRIGGER credential_authentication_release_immutable
    BEFORE UPDATE OR DELETE ON credential_authentication_release
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();
CREATE TRIGGER credential_authentication_release_cannot_be_truncated
    BEFORE TRUNCATE ON credential_authentication_release
    FOR EACH STATEMENT EXECUTE FUNCTION reject_outbox_table_truncate();

DO $migration$
DECLARE
    definition text;
BEGIN
    SELECT pg_get_functiondef('credential_pool_exhaustion_reconstruct(credential_pool_exhaustion_member)'::regprocedure)
      INTO definition;
    definition := replace(definition,
        'AND chain.credential_reference = e.profile',
        'AND chain.credential_reference = e.profile
        AND NOT EXISTS (SELECT 1 FROM credential_authentication_release released
            WHERE released.predecessor_model_call_id = chain.predecessor_model_call_id
              AND released.released_at <= e.observed_at)');
    EXECUTE definition;
END;
$migration$;
