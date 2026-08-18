-- Complete optional runner placement on both session-creation command families.

-- Supersedes the durable-command version constraint from
-- 202608030003_model_session_settings.sql.
ALTER TABLE durable_command
    DROP CONSTRAINT durable_command_storage_version_supported,
    ADD CONSTRAINT durable_command_storage_version_supported CHECK (
        (command_kind = 'create_session'
            AND storage_version IN (1, 2, 3, 4, 5, 6, 7, 8))
        OR (command_kind = 'create_session_from_imported_frontier'
            AND storage_version IN (1, 2, 3, 5, 6))
        OR (command_kind = 'replace_session_defaults'
            AND storage_version IN (1, 2, 3, 4))
        OR (command_kind = 'submit_input' AND storage_version IN (1, 2))
        OR (command_kind IN (
            'replace_session_metadata', 'decide_tool_request',
            'review_workflow', 'review_orchestration', 'compact_session',
            'goal', 'update_session_placement'
        ) AND storage_version = 1)
    );

-- Supersedes the native creation version constraint from
-- 202608030003_model_session_settings.sql.
ALTER TABLE create_session_command
    DROP CONSTRAINT create_session_command_storage_version_supported,
    ADD CONSTRAINT create_session_command_storage_version_supported
        CHECK (storage_version IN (1, 2, 3, 4, 5, 6, 7, 8));

-- Supersedes the imported creation version constraint from
-- 202608030003_model_session_settings.sql.
ALTER TABLE create_session_from_imported_frontier_command
    DROP CONSTRAINT
        create_session_from_imported_frontier_command_version_supported,
    ADD CONSTRAINT
        create_session_from_imported_frontier_command_version_supported
        CHECK (storage_version IN (1, 2, 3, 5, 6));

ALTER TABLE create_session_command
    ADD COLUMN runner_selector_kind text,
    ADD COLUMN runner_selector_runner_id uuid,
    ADD COLUMN runner_selector_capability_class runner_catalog_name,
    ADD COLUMN runner_directory_selection_kind text,
    ADD COLUMN runner_requested_working_directory runner_exact_text,
    ADD COLUMN runner_credential_profile_name runner_catalog_name,
    ADD COLUMN runner_workspace_requirement_kind text,
    ADD COLUMN runner_requested_repository_key runner_exact_text,
    ADD COLUMN runner_sandbox_profile text,
    ADD COLUMN runner_permission_override_count numeric(20, 0) NOT NULL DEFAULT 0,
    ADD CONSTRAINT create_session_command_runner_placement_shape CHECK (
        (
            storage_version < 8
            AND runner_selector_kind IS NULL
            AND runner_selector_runner_id IS NULL
            AND runner_selector_capability_class IS NULL
            AND runner_directory_selection_kind IS NULL
            AND runner_requested_working_directory IS NULL
            AND runner_credential_profile_name IS NULL
            AND runner_workspace_requirement_kind IS NULL
            AND runner_requested_repository_key IS NULL
            AND runner_sandbox_profile IS NULL
            AND runner_permission_override_count = 0
        )
        OR (
            storage_version = 8
            AND (
                (
                    runner_selector_kind IS NULL
                    AND runner_selector_runner_id IS NULL
                    AND runner_selector_capability_class IS NULL
                    AND runner_directory_selection_kind IS NULL
                    AND runner_requested_working_directory IS NULL
                    AND runner_credential_profile_name IS NULL
                    AND runner_workspace_requirement_kind IS NULL
                    AND runner_requested_repository_key IS NULL
                    AND runner_sandbox_profile IS NULL
                    AND runner_permission_override_count = 0
                )
                OR (
                    (
                        (runner_selector_kind = 'identity'
                            AND runner_selector_runner_id IS NOT NULL
                            AND runner_selector_capability_class IS NULL)
                        OR (runner_selector_kind = 'capability_class'
                            AND runner_selector_runner_id IS NULL
                            AND runner_selector_capability_class IS NOT NULL)
                    )
                    AND (
                        (runner_directory_selection_kind = 'runner_default'
                            AND runner_requested_working_directory IS NULL)
                        OR (runner_directory_selection_kind = 'exact'
                            AND runner_requested_working_directory IS NOT NULL)
                    )
                    AND (
                        (runner_workspace_requirement_kind = 'none'
                            AND runner_requested_repository_key IS NULL)
                        OR (runner_workspace_requirement_kind = 'repository_worktree'
                            AND runner_requested_repository_key IS NOT NULL)
                    )
                    AND runner_sandbox_profile IN ('ambient', 'workspace_restricted')
                    AND runner_permission_override_count BETWEEN 0 AND 64
                )
            )
        )
    );

ALTER TABLE create_session_from_imported_frontier_command
    ADD COLUMN runner_selector_kind text,
    ADD COLUMN runner_selector_runner_id uuid,
    ADD COLUMN runner_selector_capability_class runner_catalog_name,
    ADD COLUMN runner_directory_selection_kind text,
    ADD COLUMN runner_requested_working_directory runner_exact_text,
    ADD COLUMN runner_credential_profile_name runner_catalog_name,
    ADD COLUMN runner_workspace_requirement_kind text,
    ADD COLUMN runner_requested_repository_key runner_exact_text,
    ADD COLUMN runner_sandbox_profile text,
    ADD COLUMN runner_permission_override_count numeric(20, 0) NOT NULL DEFAULT 0,
    ADD CONSTRAINT imported_session_command_runner_placement_shape CHECK (
        (
            storage_version < 6
            AND runner_selector_kind IS NULL
            AND runner_selector_runner_id IS NULL
            AND runner_selector_capability_class IS NULL
            AND runner_directory_selection_kind IS NULL
            AND runner_requested_working_directory IS NULL
            AND runner_credential_profile_name IS NULL
            AND runner_workspace_requirement_kind IS NULL
            AND runner_requested_repository_key IS NULL
            AND runner_sandbox_profile IS NULL
            AND runner_permission_override_count = 0
        )
        OR (
            storage_version = 6
            AND (
                (
                    runner_selector_kind IS NULL
                    AND runner_selector_runner_id IS NULL
                    AND runner_selector_capability_class IS NULL
                    AND runner_directory_selection_kind IS NULL
                    AND runner_requested_working_directory IS NULL
                    AND runner_credential_profile_name IS NULL
                    AND runner_workspace_requirement_kind IS NULL
                    AND runner_requested_repository_key IS NULL
                    AND runner_sandbox_profile IS NULL
                    AND runner_permission_override_count = 0
                )
                OR (
                    (
                        (runner_selector_kind = 'identity'
                            AND runner_selector_runner_id IS NOT NULL
                            AND runner_selector_capability_class IS NULL)
                        OR (runner_selector_kind = 'capability_class'
                            AND runner_selector_runner_id IS NULL
                            AND runner_selector_capability_class IS NOT NULL)
                    )
                    AND (
                        (runner_directory_selection_kind = 'runner_default'
                            AND runner_requested_working_directory IS NULL)
                        OR (runner_directory_selection_kind = 'exact'
                            AND runner_requested_working_directory IS NOT NULL)
                    )
                    AND (
                        (runner_workspace_requirement_kind = 'none'
                            AND runner_requested_repository_key IS NULL)
                        OR (runner_workspace_requirement_kind = 'repository_worktree'
                            AND runner_requested_repository_key IS NOT NULL)
                    )
                    AND runner_sandbox_profile IN ('ambient', 'workspace_restricted')
                    AND runner_permission_override_count BETWEEN 0 AND 64
                )
            )
        )
    );

CREATE TABLE create_session_runner_permission_override (
    command_id uuid NOT NULL,
    tool_name text NOT NULL,
    permission_kind text NOT NULL,
    PRIMARY KEY (command_id, tool_name),
    CONSTRAINT create_session_runner_permission_override_tool_shape CHECK (
        octet_length(tool_name) BETWEEN 1 AND 64
        AND tool_name ~ '^[A-Za-z0-9_-]+$'
    ),
    CONSTRAINT create_session_runner_permission_override_closed
        CHECK (permission_kind IN ('auto', 'confirm')),
    CONSTRAINT create_session_runner_permission_override_command_fk
        FOREIGN KEY (command_id) REFERENCES create_session_command (command_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED
);

CREATE TABLE imported_session_runner_permission_override (
    command_id uuid NOT NULL,
    tool_name text NOT NULL,
    permission_kind text NOT NULL,
    PRIMARY KEY (command_id, tool_name),
    CONSTRAINT imported_session_runner_permission_override_tool_shape CHECK (
        octet_length(tool_name) BETWEEN 1 AND 64
        AND tool_name ~ '^[A-Za-z0-9_-]+$'
    ),
    CONSTRAINT imported_session_runner_permission_override_closed
        CHECK (permission_kind IN ('auto', 'confirm')),
    CONSTRAINT imported_session_runner_permission_override_command_fk
        FOREIGN KEY (command_id)
        REFERENCES create_session_from_imported_frontier_command (command_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED
);

CREATE TRIGGER create_session_runner_permission_override_is_append_only
BEFORE UPDATE OR DELETE ON create_session_runner_permission_override
FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();
CREATE TRIGGER imported_session_runner_permission_override_is_append_only
BEFORE UPDATE OR DELETE ON imported_session_runner_permission_override
FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();
CREATE TRIGGER create_session_runner_permission_override_rejects_truncate
BEFORE TRUNCATE ON create_session_runner_permission_override
FOR EACH STATEMENT EXECUTE FUNCTION reject_immutable_record_change();
CREATE TRIGGER imported_session_runner_permission_override_rejects_truncate
BEFORE TRUNCATE ON imported_session_runner_permission_override
FOR EACH STATEMENT EXECUTE FUNCTION reject_immutable_record_change();

CREATE FUNCTION assert_creation_runner_placement_complete(
    checked_kind text,
    checked_command uuid
)
RETURNS void
LANGUAGE plpgsql
AS $function$
DECLARE
    command_record record;
    placement runner_session_placement_record%ROWTYPE;
    differing_overrides bigint;
BEGIN
    IF checked_kind = 'create_session' THEN
        SELECT * INTO command_record
          FROM create_session_command
         WHERE command_id = checked_command;
    ELSIF checked_kind = 'create_session_from_imported_frontier' THEN
        SELECT * INTO command_record
          FROM create_session_from_imported_frontier_command
         WHERE command_id = checked_command;
    ELSE
        RAISE EXCEPTION 'unsupported runner-backed creation kind'
            USING ERRCODE = '23514';
    END IF;
    IF NOT FOUND THEN
        RETURN;
    END IF;

    SELECT * INTO placement
      FROM runner_session_placement_record
     WHERE session_id = command_record.created_session_id
       AND event_ordinal = 1;

    IF command_record.runner_selector_kind IS NULL THEN
        RETURN;
    END IF;
    IF NOT FOUND
       OR placement.event_kind <> 'created'
       OR placement.placement_revision <> 1
       OR placement.state_kind <> 'unpinned'
       OR NOT EXISTS (
            SELECT 1 FROM runner_current_session_placement AS placement_head
             WHERE placement_head.session_id = command_record.created_session_id
               AND placement_head.event_ordinal >= 1
       )
       OR ROW(
            command_record.runner_selector_kind,
            command_record.runner_selector_runner_id,
            command_record.runner_selector_capability_class,
            command_record.runner_directory_selection_kind,
            command_record.runner_requested_working_directory,
            command_record.runner_credential_profile_name,
            command_record.runner_workspace_requirement_kind,
            command_record.runner_requested_repository_key,
            command_record.runner_sandbox_profile,
            command_record.runner_permission_override_count
       ) IS DISTINCT FROM ROW(
            placement.selector_kind,
            placement.selector_runner_id,
            placement.selector_capability_class,
            placement.directory_selection_kind,
            placement.requested_working_directory,
            placement.requested_credential_profile_name,
            placement.workspace_requirement_kind,
            placement.requested_repository_key,
            placement.requested_sandbox_profile,
            placement.permission_override_count
       )
    THEN
        RAISE EXCEPTION 'creation runner placement differs from revision one'
            USING ERRCODE = '23514';
    END IF;

    IF checked_kind = 'create_session' THEN
        SELECT count(*) INTO differing_overrides FROM (
            (SELECT tool_name, permission_kind
               FROM create_session_runner_permission_override
              WHERE command_id = checked_command
             EXCEPT
             SELECT tool_name, permission_kind
               FROM runner_session_placement_permission_override
              WHERE session_id = command_record.created_session_id
                AND event_ordinal = 1)
            UNION ALL
            (SELECT tool_name, permission_kind
               FROM runner_session_placement_permission_override
              WHERE session_id = command_record.created_session_id
                AND event_ordinal = 1
             EXCEPT
             SELECT tool_name, permission_kind
               FROM create_session_runner_permission_override
              WHERE command_id = checked_command)
        ) AS differing;
    ELSE
        SELECT count(*) INTO differing_overrides FROM (
            (SELECT tool_name, permission_kind
               FROM imported_session_runner_permission_override
              WHERE command_id = checked_command
             EXCEPT
             SELECT tool_name, permission_kind
               FROM runner_session_placement_permission_override
              WHERE session_id = command_record.created_session_id
                AND event_ordinal = 1)
            UNION ALL
            (SELECT tool_name, permission_kind
               FROM runner_session_placement_permission_override
              WHERE session_id = command_record.created_session_id
                AND event_ordinal = 1
             EXCEPT
             SELECT tool_name, permission_kind
               FROM imported_session_runner_permission_override
              WHERE command_id = checked_command)
        ) AS differing;
    END IF;
    IF differing_overrides <> 0 THEN
        RAISE EXCEPTION 'creation runner permission overrides differ from revision one'
            USING ERRCODE = '23514';
    END IF;
END;
$function$;

CREATE FUNCTION require_native_creation_runner_placement_complete()
RETURNS trigger LANGUAGE plpgsql AS $function$
BEGIN
    PERFORM assert_creation_runner_placement_complete(
        'create_session', COALESCE(NEW.command_id, OLD.command_id));
    RETURN NULL;
END;
$function$;

CREATE FUNCTION require_imported_creation_runner_placement_complete()
RETURNS trigger LANGUAGE plpgsql AS $function$
BEGIN
    PERFORM assert_creation_runner_placement_complete(
        'create_session_from_imported_frontier',
        COALESCE(NEW.command_id, OLD.command_id));
    RETURN NULL;
END;
$function$;

CREATE FUNCTION recheck_creation_runner_placement_from_session()
RETURNS trigger LANGUAGE plpgsql AS $function$
DECLARE
    native_command uuid;
    imported_command uuid;
BEGIN
    SELECT command_id INTO native_command FROM create_session_command
     WHERE created_session_id = COALESCE(NEW.session_id, OLD.session_id);
    IF native_command IS NOT NULL THEN
        PERFORM assert_creation_runner_placement_complete(
            'create_session', native_command);
    END IF;
    SELECT command_id INTO imported_command
      FROM create_session_from_imported_frontier_command
     WHERE created_session_id = COALESCE(NEW.session_id, OLD.session_id);
    IF imported_command IS NOT NULL THEN
        PERFORM assert_creation_runner_placement_complete(
            'create_session_from_imported_frontier', imported_command);
    END IF;
    RETURN NULL;
END;
$function$;

CREATE CONSTRAINT TRIGGER create_session_requires_runner_placement
AFTER INSERT OR UPDATE OR DELETE ON create_session_command
DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
EXECUTE FUNCTION require_native_creation_runner_placement_complete();
CREATE CONSTRAINT TRIGGER imported_session_requires_runner_placement
AFTER INSERT OR UPDATE OR DELETE ON create_session_from_imported_frontier_command
DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
EXECUTE FUNCTION require_imported_creation_runner_placement_complete();
CREATE CONSTRAINT TRIGGER create_session_runner_overrides_recheck_placement
AFTER INSERT OR UPDATE OR DELETE ON create_session_runner_permission_override
DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
EXECUTE FUNCTION require_native_creation_runner_placement_complete();
CREATE CONSTRAINT TRIGGER imported_session_runner_overrides_recheck_placement
AFTER INSERT OR UPDATE OR DELETE ON imported_session_runner_permission_override
DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
EXECUTE FUNCTION require_imported_creation_runner_placement_complete();
CREATE CONSTRAINT TRIGGER runner_placement_rechecks_creation
AFTER INSERT OR UPDATE OR DELETE ON runner_session_placement_record
DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
EXECUTE FUNCTION recheck_creation_runner_placement_from_session();
CREATE CONSTRAINT TRIGGER runner_placement_overrides_recheck_creation
AFTER INSERT OR UPDATE OR DELETE ON runner_session_placement_permission_override
DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
EXECUTE FUNCTION recheck_creation_runner_placement_from_session();
