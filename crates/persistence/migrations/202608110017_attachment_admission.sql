-- Registry-first catalog and aggregate-bound rejection for blob attachments.

ALTER TABLE submit_input_command
    ADD COLUMN result_blob_digest bytea,
    ADD COLUMN result_maximum_attachment_bytes numeric(20, 0);

-- Rejected commands must retain an unknown caller-supplied digest for exact
-- replay comparison. Accepted-input parts keep their blob foreign key, while
-- the command satellite becomes the immutable pre-admission payload record.
ALTER TABLE submit_input_command_content_part
    DROP CONSTRAINT submit_input_command_content_part_blob_fk;

ALTER TABLE submit_input_command
    DROP CONSTRAINT submit_input_command_rejection_kind_closed;

ALTER TABLE submit_input_command
    ADD CONSTRAINT submit_input_command_rejection_kind_closed
    CHECK (
        rejection_kind IS NULL
        OR rejection_kind IN (
            'session_not_found',
            'no_active_turn',
            'active_turn_present',
            'active_turn_mismatch',
            'session_defaults_version_mismatch',
            'unknown_model_alias',
            'acceptance_position_exhausted',
            'safe_point_unavailable_while_stopping',
            'interrupt_already_applied',
            'interrupt_unavailable_while_awaiting_approval',
            'attachment_blob_not_found',
            'attachment_byte_budget_exceeded'
        )
    );

-- Preserve the predecessor's complete result-shape expression verbatim while
-- extending it with the two new closed rejection shapes. The migration is the
-- one-time schema transition; runtime code accepts only the resulting shape.
DO $$
DECLARE
    previous_expression text;
BEGIN
    SELECT pg_get_expr(conbin, conrelid)
      INTO previous_expression
      FROM pg_constraint
     WHERE conrelid = 'submit_input_command'::regclass
       AND conname = 'submit_input_command_result_shape';
    IF previous_expression IS NULL THEN
        RAISE EXCEPTION 'submit-input result-shape constraint is missing';
    END IF;

    ALTER TABLE submit_input_command
        DROP CONSTRAINT submit_input_command_result_shape;
    EXECUTE format(
        $shape$
        ALTER TABLE submit_input_command
        ADD CONSTRAINT submit_input_command_result_shape CHECK (
            (
                (%s)
                AND result_blob_digest IS NULL
                AND result_maximum_attachment_bytes IS NULL
            )
            OR (
                result_kind = 'rejected'
                AND rejection_kind = 'attachment_blob_not_found'
                AND result_accepted_input_id IS NULL
                AND result_turn_id IS NULL
                AND result_actual_active_turn_id IS NULL
                AND result_expected_active_turn_id IS NULL
                AND result_expected_defaults_version IS NULL
                AND result_current_defaults_version IS NULL
                AND result_unknown_alias_id IS NULL
                AND result_selected_defaults_version IS NULL
                AND result_last_position IS NULL
                AND result_existing_interrupt_command_id IS NULL
                AND result_blob_digest IS NOT NULL
                AND octet_length(result_blob_digest) = 32
                AND result_maximum_attachment_bytes IS NULL
            )
            OR (
                result_kind = 'rejected'
                AND rejection_kind = 'attachment_byte_budget_exceeded'
                AND result_accepted_input_id IS NULL
                AND result_turn_id IS NULL
                AND result_actual_active_turn_id IS NULL
                AND result_expected_active_turn_id IS NULL
                AND result_expected_defaults_version IS NULL
                AND result_current_defaults_version IS NULL
                AND result_unknown_alias_id IS NULL
                AND result_selected_defaults_version IS NULL
                AND result_last_position IS NULL
                AND result_existing_interrupt_command_id IS NULL
                AND result_blob_digest IS NULL
                AND result_maximum_attachment_bytes IS NOT NULL
                AND result_maximum_attachment_bytes BETWEEN 1 AND 18446744073709551615
            )
        )
        $shape$,
        previous_expression
    );
END;
$$;

SET CONSTRAINTS ALL IMMEDIATE;
SET CONSTRAINTS ALL DEFERRED;

ALTER TABLE durable_command DISABLE TRIGGER USER;
ALTER TABLE submit_input_command DISABLE TRIGGER USER;

ALTER TABLE submit_input_command
    DROP CONSTRAINT submit_input_command_registry_fk;
ALTER TABLE durable_command
    DROP CONSTRAINT durable_command_storage_version_supported;
ALTER TABLE submit_input_command
    DROP CONSTRAINT submit_input_command_storage_version_supported;

UPDATE durable_command SET storage_version = 4
 WHERE command_kind = 'submit_input';
UPDATE submit_input_command SET storage_version = 4;

SET CONSTRAINTS ALL IMMEDIATE;
SET CONSTRAINTS ALL DEFERRED;

ALTER TABLE durable_command
    ADD CONSTRAINT durable_command_storage_version_supported CHECK (
        (command_kind = 'create_session' AND storage_version IN (1, 2, 3, 4, 5, 6, 7))
        OR (command_kind = 'replace_session_defaults' AND storage_version IN (1, 2, 3, 4))
        OR (command_kind = 'create_session_from_imported_frontier'
            AND storage_version IN (1, 2, 3, 5))
        OR (command_kind = 'submit_input' AND storage_version = 4)
        OR (command_kind IN (
            'replace_session_metadata', 'decide_tool_request',
            'review_workflow', 'review_orchestration', 'compact_session',
            'goal', 'update_session_placement', 'register_workspace',
            'mint_git_remote', 'withdraw_git_remote') AND storage_version = 1)
    );

ALTER TABLE submit_input_command
    ADD CONSTRAINT submit_input_command_storage_version_supported
        CHECK (storage_version = 4);
ALTER TABLE submit_input_command
    ADD CONSTRAINT submit_input_command_registry_fk
        FOREIGN KEY (command_id, command_kind, storage_version)
        REFERENCES durable_command (command_id, command_kind, storage_version)
        ON UPDATE RESTRICT ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED;

ALTER TABLE durable_command ENABLE TRIGGER USER;
ALTER TABLE submit_input_command ENABLE TRIGGER USER;
