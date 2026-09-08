DO $$
DECLARE definition text;
BEGIN
    SELECT pg_get_constraintdef(oid) INTO STRICT definition FROM pg_constraint
        WHERE conrelid = 'outbox_event'::regclass AND conname = 'outbox_event_storage_version_supported';
    ALTER TABLE outbox_event DROP CONSTRAINT outbox_event_storage_version_supported;
    EXECUTE 'ALTER TABLE outbox_event ADD CONSTRAINT outbox_event_storage_version_supported CHECK (' || substring(definition FROM 7)
        || ' OR (event_kind = ''session_created'' AND storage_version = 3))';

    SELECT pg_get_constraintdef(oid) INTO STRICT definition FROM pg_constraint
        WHERE conrelid = 'session_created_outbox_event'::regclass AND conname = 'session_created_outbox_event_storage_version_supported';
    ALTER TABLE session_created_outbox_event DROP CONSTRAINT session_created_outbox_event_storage_version_supported;
    EXECUTE 'ALTER TABLE session_created_outbox_event ADD CONSTRAINT session_created_outbox_event_storage_version_supported CHECK (' || substring(definition FROM 7)
        || ' OR storage_version = 3)';
END;
$$;

ALTER TABLE session_created_outbox_event
    ADD CONSTRAINT session_created_workflow_version CHECK (creation_cause <> 'workflow' OR storage_version = 3);
