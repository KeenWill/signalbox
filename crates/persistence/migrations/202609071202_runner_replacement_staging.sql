-- A replacement retains its exact placement and candidate while runner I/O completes.

CREATE TABLE runner_replacement_stage (
    command_id uuid PRIMARY KEY REFERENCES replace_lost_runner_command(command_id),
    session_id uuid NOT NULL UNIQUE,
    source_event_ordinal numeric(20, 0) NOT NULL,
    successor_enrollment_id uuid NOT NULL,
    successor_registration_revision numeric(20, 0) NOT NULL,
    FOREIGN KEY (session_id, source_event_ordinal)
        REFERENCES runner_session_placement_record(session_id, event_ordinal),
    FOREIGN KEY (successor_enrollment_id, successor_registration_revision)
        REFERENCES runner_registration(enrollment_id, registration_revision)
);

CREATE TABLE runner_replacement_provisioning_authorization (
    authorization_id uuid PRIMARY KEY,
    command_id uuid NOT NULL UNIQUE REFERENCES replace_lost_runner_command(command_id),
    session_id uuid NOT NULL REFERENCES session(session_id),
    placement_revision numeric(20, 0) NOT NULL CHECK (placement_revision BETWEEN 1 AND 18446744073709551615),
    runner_id uuid NOT NULL,
    registration_enrollment_id uuid NOT NULL,
    registration_revision numeric(20, 0) NOT NULL,
    repository_key runner_catalog_name,
    sandbox_profile text NOT NULL CHECK (sandbox_profile IN ('ambient', 'workspace_restricted')),
    credential_profile_name runner_catalog_name,
    checkout_revision text CHECK (checkout_revision ~ '^(?:[0-9a-f]{40}|[0-9a-f]{64})$'),
    checkout_branch text,
    FOREIGN KEY (registration_enrollment_id, registration_revision, runner_id)
        REFERENCES runner_registration(enrollment_id, registration_revision, runner_id),
    CHECK (repository_key IS NOT NULL OR (credential_profile_name IS NULL AND checkout_revision IS NULL)),
    CHECK (checkout_branch IS NULL OR checkout_revision IS NOT NULL)
);

CREATE TRIGGER runner_replacement_provisioning_authorization_is_append_only
    BEFORE UPDATE OR DELETE ON runner_replacement_provisioning_authorization
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE TABLE runner_replacement_workspace_ready (
    authorization_id uuid PRIMARY KEY REFERENCES runner_replacement_provisioning_authorization(authorization_id),
    manifest_id uuid NOT NULL,
    working_directory runner_exact_text NOT NULL,
    relative_path runner_exact_text NOT NULL,
    clone_url_digest text,
    recovery_revision text CHECK (recovery_revision ~ '^(?:[0-9a-f]{40}|[0-9a-f]{64})$'),
    recovery_branch text,
    CHECK ((clone_url_digest IS NULL) = (recovery_revision IS NULL)),
    CHECK (recovery_branch IS NULL OR recovery_revision IS NOT NULL)
);

CREATE TRIGGER runner_replacement_workspace_ready_is_append_only
    BEFORE UPDATE OR DELETE ON runner_replacement_workspace_ready
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE TABLE runner_replacement_workspace_consumption (
    authorization_id uuid PRIMARY KEY REFERENCES runner_replacement_workspace_ready(authorization_id),
    command_id uuid NOT NULL UNIQUE REFERENCES replace_lost_runner_result(command_id)
        DEFERRABLE INITIALLY DEFERRED
);

CREATE TRIGGER runner_replacement_workspace_consumption_is_append_only
    BEFORE UPDATE OR DELETE ON runner_replacement_workspace_consumption
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();
