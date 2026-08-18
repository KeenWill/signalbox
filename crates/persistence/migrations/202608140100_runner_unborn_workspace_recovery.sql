-- Represent an empty repository by the branch on which its first commit will
-- be born. An unborn branch intentionally carries no object revision.
-- The receipt constraints below supersede their definitions in
-- 202608110028_runner_replacement_workspace_receipt.sql.

ALTER TABLE runner_replacement_workspace_receipt
    ALTER COLUMN revision DROP NOT NULL,
    DROP CONSTRAINT runner_replacement_workspace_receipt_revision_shape,
    DROP CONSTRAINT runner_replacement_workspace_receipt_recovery_shape,
    ADD CONSTRAINT runner_replacement_workspace_receipt_revision_shape CHECK (
        revision IS NULL
        OR revision ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'
    ),
    ADD CONSTRAINT runner_replacement_workspace_receipt_recovery_shape CHECK (
        (
            recovery_kind = 'commit'
            AND branch_name IS NULL
            AND revision IS NOT NULL
        )
        OR (
            recovery_kind IN ('branch', 'unborn_branch')
            AND branch_name IS NOT NULL
            AND octet_length(branch_name) BETWEEN 1 AND 255
            AND branch_name !~ '[[:cntrl:] ~^:?*]'
            AND position('[' IN branch_name) = 0
            AND position(chr(92) IN branch_name) = 0
            AND branch_name !~ '(^-|^/|/$|//|\.\.|@\{|\.$)'
            AND branch_name !~ '(^|/)\.'
            AND branch_name !~ '\.lock(?:/|$)'
            AND branch_name <> '@'
            AND (
                (recovery_kind = 'branch' AND revision IS NOT NULL)
                OR (recovery_kind = 'unborn_branch' AND revision IS NULL)
            )
        )
    );

-- The placement constraint below supersedes its definition in
-- 202608080100_runner_placement_loss_lifecycle.sql.
ALTER TABLE runner_session_placement_record
    DROP CONSTRAINT runner_session_placement_workspace_shape,
    ADD CONSTRAINT runner_session_placement_workspace_shape CHECK (
        (
            pinned_runner_id IS NULL
            AND workspace_repository_key IS NULL
            AND workspace_working_directory IS NULL
            AND workspace_manifest_id IS NULL
            AND workspace_placement_revision IS NULL
            AND workspace_clone_url_digest IS NULL
            AND workspace_credential_profile_name IS NULL
            AND workspace_sandbox_profile IS NULL
            AND workspace_relative_path IS NULL
            AND workspace_recovery_kind IS NULL
            AND workspace_branch_name IS NULL
            AND workspace_revision IS NULL
        )
        OR (
            pinned_runner_id IS NOT NULL
            AND workspace_requirement_kind = 'none'
            AND requested_repository_key IS NULL
            AND (
                (
                    workspace_repository_key IS NULL
                    AND workspace_working_directory IS NULL
                    AND workspace_manifest_id IS NULL
                    AND workspace_placement_revision IS NULL
                    AND workspace_clone_url_digest IS NULL
                    AND workspace_credential_profile_name IS NULL
                    AND workspace_sandbox_profile IS NULL
                    AND workspace_relative_path IS NULL
                    AND workspace_recovery_kind IS NULL
                    AND workspace_branch_name IS NULL
                    AND workspace_revision IS NULL
                    AND (
                        requested_sandbox_profile = 'ambient'
                        OR directory_selection_kind = 'exact'
                    )
                )
                OR (
                    requested_sandbox_profile = 'workspace_restricted'
                    AND directory_selection_kind = 'runner_default'
                    AND workspace_repository_key IS NULL
                    AND workspace_working_directory = pinned_working_directory
                    AND workspace_manifest_id IS NOT NULL
                    AND workspace_placement_revision IS NOT NULL
                    AND workspace_clone_url_digest IS NULL
                    AND workspace_credential_profile_name IS NULL
                    AND workspace_sandbox_profile = requested_sandbox_profile
                    AND workspace_relative_path IS NOT NULL
                    AND workspace_recovery_kind IS NULL
                    AND workspace_branch_name IS NULL
                    AND workspace_revision IS NULL
                )
            )
        )
        OR (
            pinned_runner_id IS NOT NULL
            AND workspace_requirement_kind = 'repository_worktree'
            AND requested_repository_key IS NOT NULL
            AND workspace_repository_key = requested_repository_key
            AND workspace_working_directory = pinned_working_directory
            AND workspace_manifest_id IS NOT NULL
            AND workspace_placement_revision IS NOT NULL
            AND workspace_clone_url_digest IS NOT NULL
            AND workspace_credential_profile_name IS NOT DISTINCT FROM
                requested_credential_profile_name
            AND workspace_sandbox_profile = requested_sandbox_profile
            AND workspace_relative_path IS NOT NULL
            AND workspace_recovery_kind IN (
                'commit', 'branch', 'unborn_branch'
            )
            AND (
                (
                    workspace_recovery_kind = 'commit'
                    AND workspace_branch_name IS NULL
                    AND workspace_revision IS NOT NULL
                )
                OR (
                    workspace_recovery_kind = 'branch'
                    AND workspace_branch_name IS NOT NULL
                    AND workspace_revision IS NOT NULL
                )
                OR (
                    workspace_recovery_kind = 'unborn_branch'
                    AND workspace_branch_name IS NOT NULL
                    AND workspace_revision IS NULL
                )
            )
        )
    );
