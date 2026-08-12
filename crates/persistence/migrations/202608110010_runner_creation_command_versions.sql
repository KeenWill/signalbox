-- Restore the complete durable-command version union after
-- 202608100001_workspace_and_git_remote_authority.sql reissues the closed
-- constraint. The earlier runner-creation migration cannot name command kinds
-- introduced by that later migration, while the later migration predates the
-- runner-backed creation storage versions.

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
            'goal', 'update_session_placement', 'register_workspace',
            'mint_git_remote', 'withdraw_git_remote'
        ) AND storage_version = 1)
    );
