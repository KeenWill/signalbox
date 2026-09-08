ALTER TABLE session
    ADD COLUMN creating_program_run_id uuid REFERENCES program_run_registration(run_id),
    ADD CONSTRAINT session_workflow_run_shape CHECK ((creation_cause = 'workflow') = (creating_program_run_id IS NOT NULL)),
    ADD CONSTRAINT session_workflow_identity UNIQUE (session_id, creating_program_run_id);
ALTER TABLE create_session_command
    ADD COLUMN creating_program_run_id uuid REFERENCES program_run_registration(run_id),
    ADD CONSTRAINT create_session_workflow_run_shape CHECK (
        (creation_cause = 'workflow') = (creating_program_run_id IS NOT NULL)
        AND (creating_program_run_id IS NULL OR storage_version >= 9)
    ),
    ADD CONSTRAINT create_session_workflow_matches_session FOREIGN KEY (created_session_id, creating_program_run_id)
        REFERENCES session(session_id, creating_program_run_id);

DO $$
DECLARE definition text;
BEGIN
    SELECT pg_get_constraintdef(oid) INTO STRICT definition FROM pg_constraint
        WHERE conrelid = 'session'::regclass AND conname = 'session_creation_cause_closed';
    ALTER TABLE session DROP CONSTRAINT session_creation_cause_closed;
    EXECUTE 'ALTER TABLE session ADD CONSTRAINT session_creation_cause_closed CHECK (' || substring(definition FROM 7) || ' OR creation_cause = ''workflow'')';

    SELECT pg_get_constraintdef(oid) INTO STRICT definition FROM pg_constraint
        WHERE conrelid = 'session'::regclass AND conname = 'session_creation_cause_shape';
    ALTER TABLE session DROP CONSTRAINT session_creation_cause_shape;
    EXECUTE 'ALTER TABLE session ADD CONSTRAINT session_creation_cause_shape CHECK (' || substring(definition FROM 7)
        || ' OR (creation_cause = ''workflow'' AND ancestry_kind = ''none'' AND spawning_tool_request_id IS NULL AND dispatching_module IS NULL AND dispatch_ref IS NULL))';

    SELECT pg_get_constraintdef(oid) INTO STRICT definition FROM pg_constraint
        WHERE conrelid = 'create_session_command'::regclass AND conname = 'create_session_command_creation_cause_closed';
    ALTER TABLE create_session_command DROP CONSTRAINT create_session_command_creation_cause_closed;
    EXECUTE 'ALTER TABLE create_session_command ADD CONSTRAINT create_session_command_creation_cause_closed CHECK (' || substring(definition FROM 7) || ' OR creation_cause = ''workflow'')';

    SELECT pg_get_constraintdef(oid) INTO STRICT definition FROM pg_constraint
        WHERE conrelid = 'create_session_command'::regclass AND conname = 'create_session_command_storage_version_supported';
    ALTER TABLE create_session_command DROP CONSTRAINT create_session_command_storage_version_supported;
    EXECUTE 'ALTER TABLE create_session_command ADD CONSTRAINT create_session_command_storage_version_supported CHECK (' || substring(definition FROM 7) || ' OR storage_version = 9)';

    SELECT pg_get_constraintdef(oid) INTO STRICT definition FROM pg_constraint
        WHERE conrelid = 'durable_command'::regclass AND conname = 'durable_command_storage_version_supported';
    ALTER TABLE durable_command DROP CONSTRAINT durable_command_storage_version_supported;
    EXECUTE 'ALTER TABLE durable_command ADD CONSTRAINT durable_command_storage_version_supported CHECK (' || substring(definition FROM 7) || ' OR (command_kind = ''create_session'' AND storage_version = 9))';

    SELECT pg_get_constraintdef(oid) INTO STRICT definition FROM pg_constraint
        WHERE conrelid = 'durable_command'::regclass AND conname = 'durable_command_issuer_shape';
    ALTER TABLE durable_command DROP CONSTRAINT durable_command_issuer_shape;
    EXECUTE 'ALTER TABLE durable_command ADD CONSTRAINT durable_command_issuer_shape CHECK (' || substring(definition FROM 7)
        || ' OR (issuer_kind = ''program'' AND issuer_module IS NULL AND command_kind = ''create_session'' AND storage_version = 9))';
END;
$$;

CREATE FUNCTION require_workflow_session_creation() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM create_session_command WHERE created_session_id = NEW.session_id AND creating_program_run_id = NEW.creating_program_run_id) THEN
        RAISE EXCEPTION 'workflow session requires its matching creation command' USING ERRCODE = '23503';
    END IF;
    RETURN NULL;
END;
$$;
CREATE CONSTRAINT TRIGGER workflow_session_creation_exists
AFTER INSERT ON session DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW WHEN (NEW.creation_cause = 'workflow') EXECUTE FUNCTION require_workflow_session_creation();

ALTER TABLE session_created_outbox_event
    ADD COLUMN creating_program_run_id uuid REFERENCES program_run_registration(run_id),
    ADD CONSTRAINT session_created_workflow_run_shape CHECK ((creation_cause = 'workflow') = (creating_program_run_id IS NOT NULL)),
    ADD CONSTRAINT session_created_workflow_matches_session FOREIGN KEY (session_id, creating_program_run_id)
        REFERENCES session(session_id, creating_program_run_id);
DO $$
DECLARE definition text;
BEGIN
    SELECT pg_get_constraintdef(oid) INTO STRICT definition FROM pg_constraint
        WHERE conrelid = 'session_created_outbox_event'::regclass AND conname = 'session_created_outbox_event_cause_shape';
    ALTER TABLE session_created_outbox_event DROP CONSTRAINT session_created_outbox_event_cause_shape;
    EXECUTE 'ALTER TABLE session_created_outbox_event ADD CONSTRAINT session_created_outbox_event_cause_shape CHECK (' || substring(definition FROM 7)
        || ' OR (creation_cause = ''workflow'' AND spawning_tool_request_id IS NULL AND dispatching_module IS NULL AND dispatch_ref IS NULL))';
END;
$$;
