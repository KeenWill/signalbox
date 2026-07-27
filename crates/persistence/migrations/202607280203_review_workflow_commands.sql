-- Owner-global durable receipts for process-protocol review commands.

ALTER TABLE durable_command
    DROP CONSTRAINT durable_command_kind_closed,
    DROP CONSTRAINT durable_command_storage_version_supported;

ALTER TABLE durable_command
    ADD CONSTRAINT durable_command_kind_closed
        CHECK (
            command_kind IN (
                'create_session',
                'create_session_from_imported_frontier',
                'replace_session_defaults',
                'replace_session_metadata',
                'submit_input',
                'decide_tool_request',
                'review_workflow'
            )
        ),
    ADD CONSTRAINT durable_command_storage_version_supported
        CHECK (
            (
                command_kind IN (
                    'create_session',
                    'create_session_from_imported_frontier',
                    'replace_session_defaults'
                )
                AND storage_version IN (1, 2)
            )
            OR (
                command_kind IN (
                    'replace_session_metadata',
                    'submit_input',
                    'decide_tool_request',
                    'review_workflow'
                )
                AND storage_version = 1
            )
        );

CREATE TABLE review_workflow_command (
    command_id uuid PRIMARY KEY,
    command_kind text NOT NULL,
    storage_version smallint NOT NULL,
    semantic_digest bytea NOT NULL,
    operation_kind text NOT NULL,
    result_kind text NOT NULL,
    result_target_id uuid,
    result_run_id uuid,
    result_pass_id uuid,
    result_finding_id uuid,
    result_external_link_id uuid,
    result_finding_count bigint,
    result_finding_status text,
    result_external_object_key text,

    CONSTRAINT review_workflow_command_kind_closed
        CHECK (command_kind = 'review_workflow'),
    CONSTRAINT review_workflow_command_storage_version_supported
        CHECK (storage_version = 1),
    CONSTRAINT review_workflow_command_digest_size
        CHECK (octet_length(semantic_digest) = 32),
    CONSTRAINT review_workflow_command_operation_closed
        CHECK (
            operation_kind IN (
                'create_target',
                'start_run',
                'activate_pass',
                'record_findings',
                'record_finding_event',
                'reserve_external_link',
                'attach_external_link'
            )
        ),
    CONSTRAINT review_workflow_command_result_closed
        CHECK (
            result_kind IN (
                'target_created',
                'run_started',
                'pass_activated',
                'findings_recorded',
                'finding_event_recorded',
                'external_link_reserved',
                'external_link_attached'
            )
        ),
    CONSTRAINT review_workflow_command_operation_result
        CHECK (
            (operation_kind = 'create_target' AND result_kind = 'target_created')
            OR (operation_kind = 'start_run' AND result_kind = 'run_started')
            OR (operation_kind = 'activate_pass' AND result_kind = 'pass_activated')
            OR (operation_kind = 'record_findings' AND result_kind = 'findings_recorded')
            OR (
                operation_kind = 'record_finding_event'
                AND result_kind = 'finding_event_recorded'
            )
            OR (
                operation_kind = 'reserve_external_link'
                AND result_kind = 'external_link_reserved'
            )
            OR (
                operation_kind = 'attach_external_link'
                AND result_kind = 'external_link_attached'
            )
        ),
    CONSTRAINT review_workflow_command_finding_status_closed
        CHECK (
            result_finding_status IS NULL
            OR result_finding_status IN (
                'open',
                'accepted',
                'rejected',
                'duplicate',
                'superseded',
                'stale',
                'posted',
                'fixed',
                'blocked_with_reason'
            )
        ),
    CONSTRAINT review_workflow_command_external_object_bound
        CHECK (
            result_external_object_key IS NULL
            OR octet_length(result_external_object_key) BETWEEN 1 AND 1024
        ),
    CONSTRAINT review_workflow_command_result_shape
        CHECK (
            (
                result_kind = 'target_created'
                AND result_target_id IS NOT NULL
                AND result_run_id IS NULL
                AND result_pass_id IS NULL
                AND result_finding_id IS NULL
                AND result_external_link_id IS NULL
                AND result_finding_count IS NULL
                AND result_finding_status IS NULL
                AND result_external_object_key IS NULL
            )
            OR (
                result_kind IN ('run_started', 'pass_activated')
                AND result_target_id IS NULL
                AND result_run_id IS NOT NULL
                AND result_pass_id IS NOT NULL
                AND result_finding_id IS NULL
                AND result_external_link_id IS NULL
                AND result_finding_count IS NULL
                AND result_finding_status IS NULL
                AND result_external_object_key IS NULL
            )
            OR (
                result_kind = 'findings_recorded'
                AND result_target_id IS NULL
                AND result_run_id IS NOT NULL
                AND result_pass_id IS NOT NULL
                AND result_finding_id IS NULL
                AND result_external_link_id IS NULL
                AND result_finding_count >= 0
                AND result_finding_status IS NULL
                AND result_external_object_key IS NULL
            )
            OR (
                result_kind = 'finding_event_recorded'
                AND result_target_id IS NULL
                AND result_run_id IS NULL
                AND result_pass_id IS NULL
                AND result_finding_id IS NOT NULL
                AND result_external_link_id IS NULL
                AND result_finding_count IS NULL
                AND result_finding_status IS NOT NULL
                AND result_external_object_key IS NULL
            )
            OR (
                result_kind = 'external_link_reserved'
                AND result_target_id IS NULL
                AND result_run_id IS NULL
                AND result_pass_id IS NULL
                AND result_finding_id IS NULL
                AND result_external_link_id IS NOT NULL
                AND result_finding_count IS NULL
                AND result_finding_status IS NULL
                AND result_external_object_key IS NULL
            )
            OR (
                result_kind = 'external_link_attached'
                AND result_target_id IS NULL
                AND result_run_id IS NULL
                AND result_pass_id IS NULL
                AND result_finding_id IS NULL
                AND result_external_link_id IS NOT NULL
                AND result_finding_count IS NULL
                AND result_finding_status IS NULL
                AND result_external_object_key IS NOT NULL
            )
        ),
    CONSTRAINT review_workflow_command_registry_fk
        FOREIGN KEY (command_id, command_kind, storage_version)
        REFERENCES durable_command (command_id, command_kind, storage_version)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE TRIGGER review_workflow_command_is_append_only
BEFORE UPDATE OR DELETE ON review_workflow_command
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE OR REPLACE FUNCTION require_durable_command_typed_record()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    matching_records bigint;
BEGIN
    CASE NEW.command_kind
        WHEN 'create_session' THEN
            SELECT count(*) INTO matching_records FROM create_session_command
             WHERE command_id = NEW.command_id;
        WHEN 'create_session_from_imported_frontier' THEN
            SELECT count(*) INTO matching_records
              FROM create_session_from_imported_frontier_command
             WHERE command_id = NEW.command_id;
        WHEN 'replace_session_defaults' THEN
            SELECT count(*) INTO matching_records FROM replace_session_defaults_command
             WHERE command_id = NEW.command_id;
        WHEN 'replace_session_metadata' THEN
            SELECT count(*) INTO matching_records FROM replace_session_metadata_command
             WHERE command_id = NEW.command_id;
        WHEN 'submit_input' THEN
            SELECT count(*) INTO matching_records FROM submit_input_command
             WHERE command_id = NEW.command_id;
        WHEN 'decide_tool_request' THEN
            SELECT count(*) INTO matching_records FROM decide_tool_request_command
             WHERE command_id = NEW.command_id;
        WHEN 'review_workflow' THEN
            SELECT count(*) INTO matching_records FROM review_workflow_command
             WHERE command_id = NEW.command_id;
        ELSE
            RAISE EXCEPTION 'unsupported durable command kind %', NEW.command_kind
                USING ERRCODE = '23514';
    END CASE;

    IF matching_records <> 1 THEN
        RAISE EXCEPTION
            'durable command % requires exactly one % typed record',
            NEW.command_id,
            NEW.command_kind
            USING ERRCODE = '23503';
    END IF;
    RETURN NULL;
END;
$$;

CREATE FUNCTION reject_review_workflow_command_truncate()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION 'review workflow command receipts are append-only'
        USING ERRCODE = '23514';
END;
$$;

CREATE TRIGGER review_workflow_command_truncate_is_rejected
BEFORE TRUNCATE ON review_workflow_command
FOR EACH STATEMENT
EXECUTE FUNCTION reject_review_workflow_command_truncate();
