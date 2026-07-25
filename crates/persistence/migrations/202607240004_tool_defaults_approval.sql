-- Preserve the dangerous blanket-auto posture in session defaults and the
-- immutable commands that install them.

ALTER TABLE session_defaults_version
    ADD COLUMN dangerous_tool_auto_approval text NOT NULL DEFAULT 'disabled',
    ADD CONSTRAINT session_defaults_version_tool_approval_closed
        CHECK (dangerous_tool_auto_approval IN ('disabled', 'approve_all'));

ALTER TABLE create_session_command
    ADD COLUMN dangerous_tool_auto_approval text NOT NULL DEFAULT 'disabled',
    ADD CONSTRAINT create_session_command_tool_approval_closed
        CHECK (dangerous_tool_auto_approval IN ('disabled', 'approve_all'));

ALTER TABLE replace_session_defaults_command
    ADD COLUMN dangerous_tool_auto_approval text NOT NULL DEFAULT 'disabled',
    ADD CONSTRAINT replace_session_defaults_command_tool_approval_closed
        CHECK (dangerous_tool_auto_approval IN ('disabled', 'approve_all'));

ALTER TABLE create_session_from_imported_frontier_command
    ADD COLUMN dangerous_tool_auto_approval text NOT NULL DEFAULT 'disabled',
    ADD CONSTRAINT imported_frontier_command_tool_approval_closed
        CHECK (dangerous_tool_auto_approval IN ('disabled', 'approve_all'));

ALTER TABLE create_session_command
    DROP CONSTRAINT create_session_command_initial_defaults_fk;

ALTER TABLE replace_session_defaults_command
    DROP CONSTRAINT replace_session_defaults_command_applied_defaults_fk;

ALTER TABLE create_session_from_imported_frontier_command
    DROP CONSTRAINT create_session_from_imported_frontier_command_defaults_fk;

ALTER TABLE session_defaults_version
    DROP CONSTRAINT session_defaults_version_selection_key,
    ADD CONSTRAINT session_defaults_version_selection_key
        UNIQUE (
            session_id,
            version,
            model_selection_kind,
            model_selection_reference,
            dangerous_tool_auto_approval
        );

ALTER TABLE create_session_command
    ADD CONSTRAINT create_session_command_initial_defaults_fk
        FOREIGN KEY (
            created_session_id,
            initial_defaults_version,
            model_selection_kind,
            model_selection_reference,
            dangerous_tool_auto_approval
        )
        REFERENCES session_defaults_version (
            session_id,
            version,
            model_selection_kind,
            model_selection_reference,
            dangerous_tool_auto_approval
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT;

ALTER TABLE replace_session_defaults_command
    ADD CONSTRAINT replace_session_defaults_command_applied_defaults_fk
        FOREIGN KEY (
            result_session_id,
            result_installed_version,
            model_selection_kind,
            model_selection_reference,
            dangerous_tool_auto_approval
        )
        REFERENCES session_defaults_version (
            session_id,
            version,
            model_selection_kind,
            model_selection_reference,
            dangerous_tool_auto_approval
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED;

ALTER TABLE create_session_from_imported_frontier_command
    ADD CONSTRAINT create_session_from_imported_frontier_command_defaults_fk
        FOREIGN KEY (
            created_session_id,
            initial_defaults_version,
            model_selection_kind,
            model_selection_reference,
            dangerous_tool_auto_approval
        )
        REFERENCES session_defaults_version (
            session_id,
            version,
            model_selection_kind,
            model_selection_reference,
            dangerous_tool_auto_approval
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED;
