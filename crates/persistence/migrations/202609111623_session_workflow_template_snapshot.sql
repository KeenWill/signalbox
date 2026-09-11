CREATE TABLE session_workflow_template_snapshot (
    template_name text NOT NULL,
    template_content_digest bytea NOT NULL CHECK (octet_length(template_content_digest) = 32),
    workflow_tools jsonb NOT NULL,
    PRIMARY KEY (template_name, template_content_digest)
);

CREATE TRIGGER session_workflow_template_snapshot_immutable
    BEFORE UPDATE OR DELETE ON session_workflow_template_snapshot
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER session_workflow_template_snapshot_no_truncate
    BEFORE TRUNCATE ON session_workflow_template_snapshot
    FOR EACH STATEMENT EXECUTE FUNCTION reject_immutable_record_change();
