ALTER TABLE workspace
    DROP CONSTRAINT workspace_storage_version_closed,
    ADD CONSTRAINT workspace_storage_version_closed
        CHECK (storage_version IS NULL OR storage_version IN (1, 2)),
    ADD CONSTRAINT workspace_registration_request_required
        CHECK (storage_version IS DISTINCT FROM 2 OR registration_request_root IS NOT NULL);

DO $$
DECLARE definition text;
BEGIN
    SELECT pg_get_constraintdef(oid) INTO STRICT definition FROM pg_constraint
        WHERE conrelid = 'durable_command'::regclass AND conname = 'durable_command_storage_version_supported';
    ALTER TABLE durable_command DROP CONSTRAINT durable_command_storage_version_supported;
    EXECUTE 'ALTER TABLE durable_command ADD CONSTRAINT durable_command_storage_version_supported CHECK ('
        || substring(definition FROM 7)
        || ' OR (command_kind = ''register_workspace'' AND storage_version = 2))';
END;
$$;
