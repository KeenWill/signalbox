-- Retire one completed managed-workspace release with immutable evidence.
-- Cleanup refusal and connection-loss retirement remain separate terminal
-- proofs; successful acknowledgement serializes on the cleanup enrollment
-- authority shared with connection-loss admission.

CREATE TABLE runner_workspace_release_acknowledgement (
    session_id uuid NOT NULL,
    placement_revision numeric(20, 0) NOT NULL,
    runner_id uuid NOT NULL,
    manifest_id uuid NOT NULL,

    CONSTRAINT runner_workspace_release_acknowledgement_pk
        PRIMARY KEY (session_id, placement_revision),
    CONSTRAINT runner_workspace_release_acknowledgement_positive_u64 CHECK (
        placement_revision BETWEEN 1 AND 18446744073709551615
    ),
    CONSTRAINT runner_workspace_release_acknowledgement_release_fk
        FOREIGN KEY (
            session_id,
            placement_revision,
            runner_id,
            manifest_id
        )
        REFERENCES runner_workspace_release (
            session_id,
            placement_revision,
            runner_id,
            manifest_id
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE FUNCTION require_runner_workspace_release_acknowledgement_authority()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    release runner_workspace_release%ROWTYPE;
BEGIN
    SELECT * INTO release
      FROM runner_workspace_release
     WHERE session_id = NEW.session_id
       AND placement_revision = NEW.placement_revision;

    PERFORM enrollment_id
      FROM runner_enrollment
     WHERE enrollment_id = release.enrollment_id
     FOR UPDATE;

    IF release.state_kind IS DISTINCT FROM 'pending'
       OR release.runner_id IS DISTINCT FROM NEW.runner_id
       OR release.manifest_id IS DISTINCT FROM NEW.manifest_id
       OR EXISTS (
            SELECT 1
              FROM runner_connection_loss_epoch AS loss
             WHERE loss.enrollment_id = release.enrollment_id
               AND loss.connection_epoch = release.connection_epoch
       )
    THEN
        RAISE EXCEPTION 'workspace release acknowledgement lacks live pending authority'
            USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER runner_workspace_release_acknowledgement_is_checked
AFTER INSERT ON runner_workspace_release_acknowledgement
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_runner_workspace_release_acknowledgement_authority();

CREATE TRIGGER runner_workspace_release_acknowledgement_is_append_only
BEFORE UPDATE OR DELETE ON runner_workspace_release_acknowledgement
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER runner_workspace_release_acknowledgement_rejects_truncate
BEFORE TRUNCATE ON runner_workspace_release_acknowledgement
FOR EACH STATEMENT
EXECUTE FUNCTION reject_immutable_record_change();
