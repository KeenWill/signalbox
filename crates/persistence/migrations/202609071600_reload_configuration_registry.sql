DO $$
DECLARE
    existing_expression text;
    typed_record_function text;
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

    SELECT pg_get_functiondef('require_durable_command_typed_record()'::regprocedure)
      INTO STRICT typed_record_function;
    IF position('WHEN ''reload_configuration'' THEN' IN typed_record_function) = 0 THEN
        EXECUTE replace(typed_record_function, 'CASE NEW.command_kind',
        'CASE NEW.command_kind
        WHEN ''reload_configuration'' THEN SELECT count(*) INTO matching_records FROM reload_configuration_command WHERE command_id = NEW.command_id;');
    END IF;
END;
$$;
