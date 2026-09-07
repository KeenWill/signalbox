-- Durable checked reload intent and terminal receipts.
DO $$
DECLARE
    existing_expression text;
BEGIN
    SELECT pg_get_expr(conbin, conrelid) INTO STRICT existing_expression
      FROM pg_constraint
     WHERE conrelid = 'durable_command'::regclass
       AND conname = 'durable_command_kind_closed';
    ALTER TABLE durable_command DROP CONSTRAINT durable_command_kind_closed;
    EXECUTE format(
        'ALTER TABLE durable_command ADD CONSTRAINT durable_command_kind_closed CHECK ((%s) OR command_kind = %L)',
        existing_expression, 'reload_configuration'
    );

    SELECT pg_get_expr(conbin, conrelid) INTO STRICT existing_expression
      FROM pg_constraint
     WHERE conrelid = 'durable_command'::regclass
       AND conname = 'durable_command_storage_version_supported';
    ALTER TABLE durable_command DROP CONSTRAINT durable_command_storage_version_supported;
    EXECUTE format(
        'ALTER TABLE durable_command ADD CONSTRAINT durable_command_storage_version_supported CHECK ((%s) OR (command_kind = %L AND storage_version = 1))',
        existing_expression, 'reload_configuration'
    );

END;
$$;

CREATE TABLE reload_configuration_command (
    command_id uuid PRIMARY KEY,
    command_kind text NOT NULL DEFAULT 'reload_configuration' CHECK (command_kind = 'reload_configuration'),
    storage_version smallint NOT NULL DEFAULT 1 CHECK (storage_version = 1),
    FOREIGN KEY (command_id, command_kind, storage_version)
        REFERENCES durable_command(command_id, command_kind, storage_version)
        DEFERRABLE INITIALLY DEFERRED,
    replacement_snapshot jsonb,
    prior_snapshot jsonb,
    rule_set_digest bytea,
    CHECK ((replacement_snapshot IS NULL) = (prior_snapshot IS NULL)),
    CHECK ((replacement_snapshot IS NULL) = (rule_set_digest IS NULL)),
    CHECK (replacement_snapshot IS NULL OR jsonb_typeof(replacement_snapshot) = 'object'),
    CHECK (prior_snapshot IS NULL OR jsonb_typeof(prior_snapshot) = 'object'),
    CHECK (rule_set_digest IS NULL OR octet_length(rule_set_digest) = 32)
);
CREATE TABLE reload_configuration_result (
    command_id uuid PRIMARY KEY REFERENCES reload_configuration_command(command_id),
    outcome text NOT NULL CHECK (outcome IN ('reloaded', 'failed')),
    phase text CHECK (phase IN ('read', 'validate', 'activate', 'reconcile', 'install')),
    reason text CHECK (octet_length(reason) BETWEEN 1 AND 1024),
    CHECK ((outcome = 'failed') = (phase IS NOT NULL)),
    CHECK ((outcome = 'failed') = (reason IS NOT NULL))
);
CREATE TRIGGER reload_configuration_command_is_append_only
    BEFORE UPDATE OR DELETE ON reload_configuration_command
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();
CREATE TRIGGER reload_configuration_result_is_append_only
    BEFORE UPDATE OR DELETE ON reload_configuration_result
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();
DO $$
DECLARE typed_record_function text;
BEGIN
    SELECT pg_get_functiondef('require_durable_command_typed_record()'::regprocedure)
      INTO STRICT typed_record_function;
    IF position('WHEN ''reload_configuration'' THEN' IN typed_record_function) = 0 THEN
        EXECUTE replace(typed_record_function, 'CASE NEW.command_kind',
        'CASE NEW.command_kind
        WHEN ''reload_configuration'' THEN SELECT count(*) INTO matching_records FROM reload_configuration_command WHERE command_id = NEW.command_id;');
    END IF;
END;
$$;

CREATE FUNCTION require_reload_configuration_intent() RETURNS trigger
    LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.replacement_snapshot IS NULL AND NOT EXISTS (
        SELECT 1 FROM reload_configuration_result
        WHERE command_id = NEW.command_id AND outcome = 'failed' AND phase IN ('read', 'validate')
    ) THEN
        RAISE EXCEPTION 'reload requires checked intent or a pre-effect rejection' USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;
CREATE CONSTRAINT TRIGGER reload_configuration_requires_intent
    AFTER INSERT ON reload_configuration_command DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION require_reload_configuration_intent();
