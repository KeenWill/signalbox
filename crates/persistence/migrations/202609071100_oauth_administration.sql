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
            'session_lifecycle'::text, 'provision_oauth_credential'::text, 'reprovision_oauth_credential'::text, 'delete_oauth_credential'::text
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
                'session_lifecycle'::text, 'provision_oauth_credential'::text, 'reprovision_oauth_credential'::text, 'delete_oauth_credential'::text
            ])) AND (storage_version = 1))
    );

CREATE TABLE provision_oauth_credential_command (
    command_id uuid PRIMARY KEY,
    command_kind text NOT NULL CHECK (command_kind = 'provision_oauth_credential'),
    storage_version smallint NOT NULL CHECK (storage_version = 1),
    profile text NOT NULL CHECK (octet_length(profile) BETWEEN 1 AND 256 AND profile = btrim(profile)),
    FOREIGN KEY (command_id, command_kind, storage_version)
        REFERENCES durable_command(command_id, command_kind, storage_version)
        ON UPDATE RESTRICT ON DELETE RESTRICT
);
CREATE TABLE provision_oauth_credential_result (
    command_id uuid PRIMARY KEY REFERENCES provision_oauth_credential_command(command_id),
    outcome text NOT NULL CHECK (outcome IN ('provisioned','already_provisioned','reprovisioned','deleted','already_deleted','not_provisioned','abandoned','superseded','failed')),
    reason text CHECK (reason IN ('unknown_profile','non_oauth_profile','registration_changed','device_endpoint_rejected','device_endpoint_failed','access_denied','polling_expired','token_endpoint_failed','token_response_without_identity','account_independence_failed')),
    CHECK ((outcome = 'failed') = (reason IS NOT NULL))
);
CREATE TRIGGER provision_oauth_credential_command_is_append_only
    BEFORE UPDATE OR DELETE ON provision_oauth_credential_command
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();
CREATE TRIGGER provision_oauth_credential_result_is_append_only
    BEFORE UPDATE OR DELETE ON provision_oauth_credential_result
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE TABLE reprovision_oauth_credential_command (
    command_id uuid PRIMARY KEY,
    command_kind text NOT NULL CHECK (command_kind = 'reprovision_oauth_credential'),
    storage_version smallint NOT NULL CHECK (storage_version = 1),
    profile text NOT NULL CHECK (octet_length(profile) BETWEEN 1 AND 256 AND profile = btrim(profile)),
    FOREIGN KEY (command_id, command_kind, storage_version)
        REFERENCES durable_command(command_id, command_kind, storage_version)
        ON UPDATE RESTRICT ON DELETE RESTRICT
);
CREATE TABLE reprovision_oauth_credential_result (
    command_id uuid PRIMARY KEY REFERENCES reprovision_oauth_credential_command(command_id),
    outcome text NOT NULL CHECK (outcome IN ('provisioned','already_provisioned','reprovisioned','deleted','already_deleted','not_provisioned','abandoned','superseded','failed')),
    reason text CHECK (reason IN ('unknown_profile','non_oauth_profile','registration_changed','device_endpoint_rejected','device_endpoint_failed','access_denied','polling_expired','token_endpoint_failed','token_response_without_identity','account_independence_failed')),
    CHECK ((outcome = 'failed') = (reason IS NOT NULL))
);
CREATE TRIGGER reprovision_oauth_credential_command_is_append_only
    BEFORE UPDATE OR DELETE ON reprovision_oauth_credential_command
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();
CREATE TRIGGER reprovision_oauth_credential_result_is_append_only
    BEFORE UPDATE OR DELETE ON reprovision_oauth_credential_result
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE TABLE delete_oauth_credential_command (
    command_id uuid PRIMARY KEY,
    command_kind text NOT NULL CHECK (command_kind = 'delete_oauth_credential'),
    storage_version smallint NOT NULL CHECK (storage_version = 1),
    profile text NOT NULL CHECK (octet_length(profile) BETWEEN 1 AND 256 AND profile = btrim(profile)),
    FOREIGN KEY (command_id, command_kind, storage_version)
        REFERENCES durable_command(command_id, command_kind, storage_version)
        ON UPDATE RESTRICT ON DELETE RESTRICT
);
CREATE TABLE delete_oauth_credential_result (
    command_id uuid PRIMARY KEY REFERENCES delete_oauth_credential_command(command_id),
    outcome text NOT NULL CHECK (outcome IN ('provisioned','already_provisioned','reprovisioned','deleted','already_deleted','not_provisioned','abandoned','superseded','failed')),
    reason text CHECK (reason IN ('unknown_profile','non_oauth_profile','registration_changed','device_endpoint_rejected','device_endpoint_failed','access_denied','polling_expired','token_endpoint_failed','token_response_without_identity','account_independence_failed')),
    CHECK ((outcome = 'failed') = (reason IS NOT NULL))
);
CREATE TRIGGER delete_oauth_credential_command_is_append_only
    BEFORE UPDATE OR DELETE ON delete_oauth_credential_command
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();
CREATE TRIGGER delete_oauth_credential_result_is_append_only
    BEFORE UPDATE OR DELETE ON delete_oauth_credential_result
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
        WHEN 'provision_oauth_credential' THEN SELECT count(*) INTO matching_records FROM provision_oauth_credential_command WHERE command_id = NEW.command_id;
        WHEN 'reprovision_oauth_credential' THEN SELECT count(*) INTO matching_records FROM reprovision_oauth_credential_command WHERE command_id = NEW.command_id;
        WHEN 'delete_oauth_credential' THEN SELECT count(*) INTO matching_records FROM delete_oauth_credential_command WHERE command_id = NEW.command_id;
        ELSE RAISE EXCEPTION 'unsupported durable command kind %', NEW.command_kind USING ERRCODE = '23514';
    END CASE;
    IF matching_records <> 1 THEN
        RAISE EXCEPTION 'durable command % requires exactly one % typed record', NEW.command_id, NEW.command_kind USING ERRCODE = '23503';
    END IF;
    RETURN NULL;
END;
$$;
