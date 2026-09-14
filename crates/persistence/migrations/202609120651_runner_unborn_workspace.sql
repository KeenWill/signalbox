-- An unborn repository retains its branch without inventing a commit revision.
ALTER TABLE runner_session_placement_record
    DROP CONSTRAINT runner_session_placement_workspace_shape,
    ADD CONSTRAINT runner_session_placement_workspace_shape CHECK ((((pinned_runner_id IS NULL) AND (workspace_repository_key IS NULL) AND (workspace_working_directory IS NULL) AND (workspace_manifest_id IS NULL) AND (workspace_placement_revision IS NULL) AND (workspace_clone_url_digest IS NULL) AND (workspace_credential_profile_name IS NULL) AND (workspace_sandbox_profile IS NULL) AND (workspace_relative_path IS NULL) AND (workspace_recovery_kind IS NULL) AND (workspace_branch_name IS NULL) AND (workspace_revision IS NULL)) OR ((pinned_runner_id IS NOT NULL) AND (workspace_requirement_kind = 'none'::text) AND (requested_repository_key IS NULL) AND (((workspace_repository_key IS NULL) AND (workspace_working_directory IS NULL) AND (workspace_manifest_id IS NULL) AND (workspace_placement_revision IS NULL) AND (workspace_clone_url_digest IS NULL) AND (workspace_credential_profile_name IS NULL) AND (workspace_sandbox_profile IS NULL) AND (workspace_relative_path IS NULL) AND (workspace_recovery_kind IS NULL) AND (workspace_branch_name IS NULL) AND (workspace_revision IS NULL) AND ((requested_sandbox_profile = 'ambient'::text) OR (directory_selection_kind = 'exact'::text))) OR ((requested_sandbox_profile = 'workspace_restricted'::text) AND (directory_selection_kind = 'runner_default'::text) AND (workspace_repository_key IS NULL) AND ((workspace_working_directory)::text = (pinned_working_directory)::text) AND (workspace_manifest_id IS NOT NULL) AND (workspace_placement_revision IS NOT NULL) AND (workspace_clone_url_digest IS NULL) AND (workspace_credential_profile_name IS NULL) AND (workspace_sandbox_profile = requested_sandbox_profile) AND (workspace_relative_path IS NOT NULL) AND (workspace_recovery_kind IS NULL) AND (workspace_branch_name IS NULL) AND (workspace_revision IS NULL)))) OR ((pinned_runner_id IS NOT NULL) AND (workspace_requirement_kind = 'repository_worktree'::text) AND (requested_repository_key IS NOT NULL) AND ((workspace_repository_key)::text = (requested_repository_key)::text) AND ((workspace_working_directory)::text = (pinned_working_directory)::text) AND (workspace_manifest_id IS NOT NULL) AND (workspace_placement_revision IS NOT NULL) AND (workspace_clone_url_digest IS NOT NULL) AND (NOT ((workspace_credential_profile_name)::text IS DISTINCT FROM (requested_credential_profile_name)::text)) AND (workspace_sandbox_profile = requested_sandbox_profile) AND (workspace_relative_path IS NOT NULL) AND (((workspace_recovery_kind = 'unborn_branch'::text) AND (workspace_branch_name IS NOT NULL) AND (workspace_revision IS NULL)) OR ((workspace_revision IS NOT NULL) AND (((workspace_recovery_kind = 'commit'::text) AND (workspace_branch_name IS NULL)) OR ((workspace_recovery_kind = 'branch'::text) AND (workspace_branch_name IS NOT NULL))))))));

ALTER TABLE runner_replacement_provisioning_authorization
    DROP CONSTRAINT runner_replacement_provisioning_authorization_check,
    DROP CONSTRAINT runner_replacement_provisioning_authorization_check1,
    ADD CONSTRAINT runner_replacement_provisioning_authorization_recovery_shape
        CHECK (repository_key IS NOT NULL OR
            (credential_profile_name IS NULL AND checkout_revision IS NULL AND checkout_branch IS NULL));

ALTER TABLE runner_replacement_workspace_ready
    DROP CONSTRAINT runner_replacement_workspace_ready_check,
    DROP CONSTRAINT runner_replacement_workspace_ready_check1,
    ADD CONSTRAINT runner_replacement_workspace_ready_recovery_shape
        CHECK ((clone_url_digest IS NULL AND recovery_revision IS NULL AND recovery_branch IS NULL)
            OR (clone_url_digest IS NOT NULL AND (recovery_revision IS NOT NULL OR recovery_branch IS NOT NULL)));

ALTER TABLE runner_replacement_provisioning_authorization
    DROP CONSTRAINT runner_replacement_repository_recovery_pair,
    ADD CONSTRAINT runner_replacement_repository_recovery_pair
        CHECK ((repository_key IS NULL) = (checkout_revision IS NULL AND checkout_branch IS NULL));

CREATE TABLE runner_replacement_workspace_release_reauthorization (
    authorization_id uuid NOT NULL REFERENCES runner_replacement_workspace_release(authorization_id),
    connection_epoch numeric(20,0) NOT NULL CHECK (connection_epoch >= 1),
    PRIMARY KEY (authorization_id, connection_epoch)
);

CREATE TRIGGER runner_replacement_workspace_release_reauthorization_is_append_only
    BEFORE UPDATE OR DELETE ON runner_replacement_workspace_release_reauthorization
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();
