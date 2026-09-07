CREATE TABLE runner_replacement_provisioning_failure (
    authorization_id uuid PRIMARY KEY REFERENCES runner_replacement_provisioning_authorization(authorization_id),
    failure_kind text NOT NULL CHECK (failure_kind IN ('credential_unavailable', 'repository_unavailable', 'sandbox_unavailable', 'workspace_conflict')),
    detail jsonb NOT NULL CHECK (jsonb_typeof(detail) = 'object')
);

CREATE TRIGGER runner_replacement_provisioning_failure_is_append_only
    BEFORE UPDATE OR DELETE ON runner_replacement_provisioning_failure
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE TABLE runner_replacement_workspace_release (
    authorization_id uuid PRIMARY KEY REFERENCES runner_replacement_workspace_ready(authorization_id),
    connection_epoch numeric(20,0) NOT NULL
);

CREATE TRIGGER runner_replacement_workspace_release_is_append_only
    BEFORE UPDATE OR DELETE ON runner_replacement_workspace_release
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE TABLE runner_replacement_workspace_released (
    authorization_id uuid PRIMARY KEY REFERENCES runner_replacement_workspace_release(authorization_id)
);

CREATE TRIGGER runner_replacement_workspace_released_is_append_only
    BEFORE UPDATE OR DELETE ON runner_replacement_workspace_released
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();
