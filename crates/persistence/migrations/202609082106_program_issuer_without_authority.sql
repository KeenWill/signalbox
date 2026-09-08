DO $$
DECLARE definition text;
BEGIN
    SELECT pg_get_constraintdef(oid) INTO STRICT definition FROM pg_constraint
        WHERE conrelid = 'durable_command'::regclass AND conname = 'durable_command_issuer_shape';
    ALTER TABLE durable_command DROP CONSTRAINT durable_command_issuer_shape;
    EXECUTE 'ALTER TABLE durable_command ADD CONSTRAINT durable_command_issuer_shape CHECK ('
        || substring(definition FROM 7)
        || ' OR (issuer_kind = ''program'' AND issuer_module IS NULL AND command_kind = ''submit_input'' AND storage_version = 4))';
END;
$$;
