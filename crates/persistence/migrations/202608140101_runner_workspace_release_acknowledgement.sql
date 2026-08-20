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
    retired runner_session_placement_record%ROWTYPE;
    successor runner_session_placement_record%ROWTYPE;
    current_placement_ordinal numeric(20, 0);
    enrollment runner_enrollment%ROWTYPE;
    connection_head runner_connection_authority_head%ROWTYPE;
    connection runner_connection_event%ROWTYPE;
    head_connection runner_connection_event%ROWTYPE;
BEGIN
    SELECT * INTO release
      FROM runner_workspace_release
     WHERE session_id = NEW.session_id
       AND placement_revision = NEW.placement_revision;

    SELECT * INTO enrollment
      FROM runner_enrollment
     WHERE enrollment_id = release.enrollment_id
     FOR UPDATE;
    SELECT * INTO retired
      FROM runner_session_placement_record
     WHERE session_id = release.session_id
       AND event_ordinal = release.retired_placement_event_ordinal;
    SELECT * INTO successor
      FROM runner_session_placement_record
     WHERE session_id = release.session_id
       AND event_ordinal = release.successor_placement_event_ordinal;
    SELECT event_ordinal INTO current_placement_ordinal
      FROM runner_current_session_placement
     WHERE session_id = release.session_id;
    SELECT * INTO connection_head
      FROM runner_connection_authority_head
     WHERE enrollment_id = release.enrollment_id;
    SELECT * INTO connection
      FROM runner_connection_event
     WHERE enrollment_id = release.enrollment_id
       AND connection_epoch = release.connection_epoch
       AND event_ordinal = release.connection_event_ordinal;
    SELECT * INTO head_connection
      FROM runner_connection_event
     WHERE enrollment_id = connection_head.enrollment_id
       AND connection_epoch = connection_head.connection_epoch
       AND event_ordinal = connection_head.connection_event_ordinal;

    IF release.state_kind IS DISTINCT FROM 'pending'
       OR release.runner_id IS DISTINCT FROM NEW.runner_id
       OR release.manifest_id IS DISTINCT FROM NEW.manifest_id
       OR current_placement_ordinal IS DISTINCT FROM
            release.successor_placement_event_ordinal
       OR retired.event_kind IS DISTINCT FROM 'runner_lost'
       OR retired.state_kind IS DISTINCT FROM 'runner_lost'
       OR retired.placement_revision IS DISTINCT FROM
            release.placement_revision
       OR retired.loss_source_kind IS DISTINCT FROM 'registration'
       OR retired.lost_runner_id IS DISTINCT FROM release.runner_id
       OR retired.pinned_runner_id IS DISTINCT FROM release.runner_id
       OR retired.registration_enrollment_id IS DISTINCT FROM
            release.enrollment_id
       OR retired.workspace_manifest_id IS DISTINCT FROM release.manifest_id
       OR retired.workspace_placement_revision IS DISTINCT FROM
            release.placement_revision
       OR successor.event_kind IS DISTINCT FROM 'runner_replaced'
       OR successor.state_kind IS DISTINCT FROM 'pinned'
       OR successor.event_ordinal IS DISTINCT FROM
            release.successor_placement_event_ordinal
       OR successor.placement_revision IS DISTINCT FROM
            release.placement_revision + 1
       OR successor.pinned_runner_id IS DISTINCT FROM release.runner_id
       OR successor.registration_enrollment_id IS DISTINCT FROM
            release.enrollment_id
       OR successor.workspace_manifest_id IS NULL
       OR successor.workspace_manifest_id = release.manifest_id
       OR successor.workspace_placement_revision IS DISTINCT FROM
            successor.placement_revision
       OR enrollment.runner_id IS DISTINCT FROM release.runner_id
       OR enrollment.state_kind IS DISTINCT FROM 'active'
       OR connection_head.connection_epoch IS DISTINCT FROM
            release.connection_epoch
       OR connection.state_kind IS DISTINCT FROM 'connected'
       OR head_connection.state_kind IS DISTINCT FROM 'connected'
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
