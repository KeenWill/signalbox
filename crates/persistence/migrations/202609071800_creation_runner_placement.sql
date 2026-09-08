-- Optional runner placement is a caller-supplied creation payload field.
ALTER TABLE create_session_command
    DROP CONSTRAINT create_session_command_storage_version_supported,
    ADD CONSTRAINT create_session_command_storage_version_supported
        CHECK (storage_version IN (1, 2, 3, 4, 6, 7, 8));
ALTER TABLE create_session_from_imported_frontier_command
    DROP CONSTRAINT create_session_from_imported_frontier_command_version_supported,
    ADD CONSTRAINT create_session_from_imported_frontier_command_version_supported
        CHECK (storage_version IN (1, 2, 3, 5, 6));

DO $$
DECLARE
    subject text;
    introduced smallint;
BEGIN
    FOREACH subject IN ARRAY ARRAY['create_session_command', 'create_session_from_imported_frontier_command'] LOOP
        introduced := CASE subject WHEN 'create_session_command' THEN 8 ELSE 6 END;
        EXECUTE format('ALTER TABLE %I
            ADD COLUMN runner_selector_kind text,
            ADD COLUMN runner_selector_id uuid,
            ADD COLUMN runner_selector_class text,
            ADD COLUMN runner_directory_kind text,
            ADD COLUMN runner_directory text,
            ADD COLUMN runner_credential_profile text,
            ADD COLUMN runner_workspace_kind text,
            ADD COLUMN runner_repository text,
            ADD COLUMN runner_sandbox text,
            ADD COLUMN runner_permission_overrides jsonb,
            ADD CONSTRAINT creation_runner_placement_versioned
                CHECK (runner_selector_kind IS NULL OR storage_version >= %s),
            ADD CONSTRAINT creation_runner_placement_shape CHECK (
                num_nonnulls(runner_selector_kind, runner_selector_id, runner_selector_class,
                    runner_directory_kind, runner_directory, runner_credential_profile,
                    runner_workspace_kind, runner_repository, runner_sandbox,
                    runner_permission_overrides) = 0
                OR (
                    runner_selector_kind IS NOT NULL
                    AND ((runner_selector_kind = ''identity'' AND runner_selector_id IS NOT NULL AND runner_selector_class IS NULL)
                        OR (runner_selector_kind = ''capability_class'' AND runner_selector_id IS NULL AND runner_selector_class IS NOT NULL))
                    AND runner_directory_kind IS NOT NULL
                    AND ((runner_directory_kind = ''runner_default'' AND runner_directory IS NULL)
                        OR (runner_directory_kind = ''exact'' AND runner_directory IS NOT NULL))
                    AND runner_workspace_kind IS NOT NULL
                    AND ((runner_workspace_kind = ''none'' AND runner_repository IS NULL)
                        OR (runner_workspace_kind = ''repository_worktree'' AND runner_repository IS NOT NULL))
                    AND runner_sandbox IS NOT NULL
                    AND runner_sandbox IN (''ambient'', ''workspace_restricted'')
                    AND runner_permission_overrides IS NOT NULL
                    AND jsonb_typeof(runner_permission_overrides) = ''object''
                    AND NOT jsonb_path_exists(runner_permission_overrides, ''$.* ? (@ != "auto" && @ != "confirm")'')
                )
            )', subject, introduced);
    END LOOP;
END;
$$;

DO $$
DECLARE definition text;
BEGIN
    SELECT pg_get_constraintdef(oid) INTO STRICT definition FROM pg_constraint
        WHERE conrelid = 'durable_command'::regclass AND conname = 'durable_command_storage_version_supported';
    ALTER TABLE durable_command DROP CONSTRAINT durable_command_storage_version_supported;
    EXECUTE 'ALTER TABLE durable_command ADD CONSTRAINT durable_command_storage_version_supported CHECK ('
        || substring(definition FROM 7)
        || ' OR (command_kind = ''create_session'' AND storage_version = 8)'
        || ' OR (command_kind = ''create_session_from_imported_frontier'' AND storage_version = 6))';
END;
$$;
