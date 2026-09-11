CREATE TABLE session_workflow_registration_source (
    request_id uuid PRIMARY KEY REFERENCES tool_request(request_id),
    source bytea NOT NULL,
    artifact text NOT NULL
);

CREATE TRIGGER session_workflow_registration_source_immutable
    BEFORE UPDATE OR DELETE ON session_workflow_registration_source
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER session_workflow_registration_source_no_truncate
    BEFORE TRUNCATE ON session_workflow_registration_source
    FOR EACH STATEMENT EXECUTE FUNCTION reject_immutable_record_change();
