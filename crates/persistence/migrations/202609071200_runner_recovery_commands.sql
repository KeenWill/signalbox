-- Durable runner recovery command requests and terminal receipts.

ALTER TABLE durable_command
    DROP CONSTRAINT durable_command_kind_closed;

ALTER TABLE durable_command
    ADD CONSTRAINT durable_command_kind_closed CHECK (
        command_kind = ANY (ARRAY[
            'create_session'::text,
            'create_session_from_imported_frontier'::text,
            'replace_session_defaults'::text,
            'replace_session_metadata'::text,
            'submit_input'::text,
            'decide_tool_request'::text,
            'override_denied_tool_request'::text,
            'review_workflow'::text,
            'review_orchestration'::text,
            'compact_session'::text,
            'goal'::text,
            'update_session_placement'::text,
            'register_workspace'::text,
            'mint_git_remote'::text,
            'withdraw_git_remote'::text,
            'session_lifecycle'::text,
            'replace_lost_runner'::text,
            'abandon_lost_runner'::text,
            'promote_pending_runner'::text
        ])
    );

ALTER TABLE durable_command
    DROP CONSTRAINT durable_command_storage_version_supported;

ALTER TABLE durable_command
    ADD CONSTRAINT durable_command_storage_version_supported CHECK (
        ((command_kind = 'create_session'::text)
            AND (storage_version = ANY (ARRAY[1, 2, 3, 4, 5, 6, 7])))
        OR ((command_kind = 'replace_session_defaults'::text)
            AND (storage_version = ANY (ARRAY[1, 2, 3, 4])))
        OR ((command_kind = 'create_session_from_imported_frontier'::text)
            AND (storage_version = ANY (ARRAY[1, 2, 3, 5])))
        OR ((command_kind = 'submit_input'::text) AND (storage_version = 3))
        OR ((command_kind = ANY (ARRAY[
                'replace_session_metadata'::text,
                'decide_tool_request'::text,
                'override_denied_tool_request'::text,
                'review_workflow'::text,
                'review_orchestration'::text,
                'compact_session'::text,
                'goal'::text,
                'update_session_placement'::text,
                'register_workspace'::text,
                'mint_git_remote'::text,
                'withdraw_git_remote'::text,
                'session_lifecycle'::text,
            'replace_lost_runner'::text,
            'abandon_lost_runner'::text,
            'promote_pending_runner'::text
            ])) AND (storage_version = 1))
    );


CREATE TABLE replace_lost_runner_command (
    command_id uuid PRIMARY KEY,
    command_kind text NOT NULL CHECK (command_kind = 'replace_lost_runner'),
    storage_version smallint NOT NULL CHECK (storage_version = 1),
    session_id uuid NOT NULL,
    revision text CHECK (revision ~ '^(?:[0-9a-f]{40}|[0-9a-f]{64})$'),
    FOREIGN KEY (command_id, command_kind, storage_version)
        REFERENCES durable_command(command_id, command_kind, storage_version)
        DEFERRABLE INITIALLY DEFERRED
);

CREATE TRIGGER replace_lost_runner_command_is_append_only
    BEFORE UPDATE OR DELETE ON replace_lost_runner_command
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE TABLE replace_lost_runner_result (
    command_id uuid PRIMARY KEY REFERENCES replace_lost_runner_command(command_id),
    result_kind text NOT NULL CHECK (result_kind IN ('applied', 'rejected')),
    rejection_kind text CHECK (rejection_kind IN ('session_not_found', 'placement_not_lost', 'existing_control_required', 'pending_runner_not_found', 'runner_unavailable', 'replacement_pending', 'placement_unavailable', 'revision_without_repository', 'provisioning_failed')),
    runner_id uuid,
    placement_revision numeric(20, 0) CHECK (placement_revision BETWEEN 1 AND 18446744073709551615),
    CHECK (((result_kind = 'rejected') = (rejection_kind IS NOT NULL))
        AND ((result_kind = 'applied') = (runner_id IS NOT NULL))
        AND ((result_kind = 'applied') = (placement_revision IS NOT NULL)))
);

CREATE TRIGGER replace_lost_runner_result_is_append_only
    BEFORE UPDATE OR DELETE ON replace_lost_runner_result
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE TABLE abandon_lost_runner_command (
    command_id uuid PRIMARY KEY,
    command_kind text NOT NULL CHECK (command_kind = 'abandon_lost_runner'),
    storage_version smallint NOT NULL CHECK (storage_version = 1),
    session_id uuid NOT NULL,
    FOREIGN KEY (command_id, command_kind, storage_version)
        REFERENCES durable_command(command_id, command_kind, storage_version)
        DEFERRABLE INITIALLY DEFERRED
);

CREATE TRIGGER abandon_lost_runner_command_is_append_only
    BEFORE UPDATE OR DELETE ON abandon_lost_runner_command
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE TABLE abandon_lost_runner_result (
    command_id uuid PRIMARY KEY REFERENCES abandon_lost_runner_command(command_id),
    result_kind text NOT NULL CHECK (result_kind IN ('applied', 'rejected')),
    rejection_kind text CHECK (rejection_kind IN ('session_not_found', 'placement_not_lost', 'existing_control_required', 'pending_runner_not_found', 'runner_unavailable', 'replacement_pending', 'placement_unavailable', 'revision_without_repository', 'provisioning_failed')),
    CHECK (((result_kind = 'rejected') = (rejection_kind IS NOT NULL)))
);

CREATE TRIGGER abandon_lost_runner_result_is_append_only
    BEFORE UPDATE OR DELETE ON abandon_lost_runner_result
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE TABLE promote_pending_runner_command (
    command_id uuid PRIMARY KEY,
    command_kind text NOT NULL CHECK (command_kind = 'promote_pending_runner'),
    storage_version smallint NOT NULL CHECK (storage_version = 1),
    enrollment_request_id uuid NOT NULL,
    FOREIGN KEY (command_id, command_kind, storage_version)
        REFERENCES durable_command(command_id, command_kind, storage_version)
        DEFERRABLE INITIALLY DEFERRED
);

CREATE TRIGGER promote_pending_runner_command_is_append_only
    BEFORE UPDATE OR DELETE ON promote_pending_runner_command
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE TABLE promote_pending_runner_result (
    command_id uuid PRIMARY KEY REFERENCES promote_pending_runner_command(command_id),
    result_kind text NOT NULL CHECK (result_kind IN ('applied', 'rejected')),
    rejection_kind text CHECK (rejection_kind IN ('session_not_found', 'placement_not_lost', 'existing_control_required', 'pending_runner_not_found', 'runner_unavailable', 'replacement_pending', 'placement_unavailable', 'revision_without_repository', 'provisioning_failed')),
    runner_id uuid,
    CHECK (((result_kind = 'rejected') = (rejection_kind IS NOT NULL))
        AND ((result_kind = 'applied') = (runner_id IS NOT NULL)))
);

CREATE TRIGGER promote_pending_runner_result_is_append_only
    BEFORE UPDATE OR DELETE ON promote_pending_runner_result
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE OR REPLACE FUNCTION require_durable_command_typed_record() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE matching_records bigint;
BEGIN
    IF NEW.command_kind <> 'review_orchestration' AND EXISTS (
        SELECT 1 FROM review_orchestration_command_recovery
         WHERE command_id = NEW.command_id
    ) THEN
        RAISE EXCEPTION 'durable command % is reserved by review orchestration recovery', NEW.command_id
            USING ERRCODE = '23505';
    END IF;
    CASE NEW.command_kind
        WHEN 'create_session' THEN SELECT count(*) INTO matching_records FROM create_session_command WHERE command_id = NEW.command_id;
        WHEN 'create_session_from_imported_frontier' THEN SELECT count(*) INTO matching_records FROM create_session_from_imported_frontier_command WHERE command_id = NEW.command_id;
        WHEN 'replace_session_defaults' THEN SELECT count(*) INTO matching_records FROM replace_session_defaults_command WHERE command_id = NEW.command_id;
        WHEN 'replace_session_metadata' THEN SELECT count(*) INTO matching_records FROM replace_session_metadata_command WHERE command_id = NEW.command_id;
        WHEN 'submit_input' THEN SELECT count(*) INTO matching_records FROM submit_input_command WHERE command_id = NEW.command_id;
        WHEN 'decide_tool_request' THEN SELECT count(*) INTO matching_records FROM decide_tool_request_command WHERE command_id = NEW.command_id;
        WHEN 'override_denied_tool_request' THEN SELECT count(*) INTO matching_records FROM override_denied_tool_request_command WHERE command_id = NEW.command_id;
        WHEN 'review_workflow' THEN SELECT count(*) INTO matching_records FROM review_workflow_command WHERE command_id = NEW.command_id;
        WHEN 'review_orchestration' THEN SELECT (SELECT count(*) FROM review_orchestration_command WHERE command_id = NEW.command_id) + (SELECT count(*) FROM review_orchestration_command_intent WHERE command_id = NEW.command_id) INTO matching_records;
        WHEN 'compact_session' THEN SELECT count(*) INTO matching_records FROM compact_session_command WHERE command_id = NEW.command_id;
        WHEN 'goal' THEN SELECT count(*) INTO matching_records FROM goal_command WHERE command_id = NEW.command_id;
        WHEN 'update_session_placement' THEN SELECT count(*) INTO matching_records FROM update_session_placement_command WHERE command_id = NEW.command_id;
        WHEN 'register_workspace' THEN SELECT count(*) INTO matching_records FROM workspace WHERE command_id = NEW.command_id;
        WHEN 'mint_git_remote' THEN SELECT count(*) INTO matching_records FROM configured_git_remote_mint WHERE command_id = NEW.command_id;
        WHEN 'withdraw_git_remote' THEN SELECT count(*) INTO matching_records FROM configured_git_remote_withdrawal WHERE command_id = NEW.command_id;
        WHEN 'session_lifecycle' THEN SELECT count(*) INTO matching_records FROM session_lifecycle_command WHERE command_id = NEW.command_id;
        WHEN 'replace_lost_runner' THEN SELECT count(*) INTO matching_records FROM replace_lost_runner_command WHERE command_id = NEW.command_id;
        WHEN 'abandon_lost_runner' THEN SELECT count(*) INTO matching_records FROM abandon_lost_runner_command WHERE command_id = NEW.command_id;
        WHEN 'promote_pending_runner' THEN SELECT count(*) INTO matching_records FROM promote_pending_runner_command WHERE command_id = NEW.command_id;
        ELSE RAISE EXCEPTION 'unsupported durable command kind %', NEW.command_kind USING ERRCODE = '23514';
    END CASE;
    IF matching_records <> 1 THEN
        RAISE EXCEPTION 'durable command % requires exactly one % typed record', NEW.command_id, NEW.command_kind USING ERRCODE = '23503';
    END IF;
    RETURN NULL;
END;
$$;

CREATE FUNCTION require_runner_recovery_terminal_result() RETURNS trigger
    LANGUAGE plpgsql AS $$
BEGIN
    IF TG_TABLE_NAME = 'abandon_lost_runner_command' AND NOT EXISTS (
        SELECT 1 FROM abandon_lost_runner_result WHERE command_id = NEW.command_id
    ) OR TG_TABLE_NAME = 'promote_pending_runner_command' AND NOT EXISTS (
        SELECT 1 FROM promote_pending_runner_result WHERE command_id = NEW.command_id
    ) THEN
        RAISE EXCEPTION 'runner recovery command requires its terminal result'
            USING ERRCODE = '23503';
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER abandon_lost_runner_requires_terminal_result
    AFTER INSERT ON abandon_lost_runner_command DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION require_runner_recovery_terminal_result();

CREATE CONSTRAINT TRIGGER promote_pending_runner_requires_terminal_result
    AFTER INSERT ON promote_pending_runner_command DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION require_runner_recovery_terminal_result();
