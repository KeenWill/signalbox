-- Retain the exact pending release for a managed predecessor workspace.
-- The production enqueue and terminal acknowledgement transactions follow in
-- later slices; this relation supplies their checked durable correlation.

CREATE TABLE runner_workspace_release (
    session_id uuid NOT NULL,
    placement_revision numeric(20, 0) NOT NULL,
    runner_id uuid NOT NULL,
    manifest_id uuid NOT NULL,
    retired_placement_event_ordinal numeric(20, 0) NOT NULL,
    successor_placement_event_ordinal numeric(20, 0) NOT NULL,
    enrollment_id uuid NOT NULL,
    connection_epoch numeric(20, 0) NOT NULL,
    connection_event_ordinal numeric(20, 0) NOT NULL,
    state_kind text NOT NULL,

    CONSTRAINT runner_workspace_release_pk
        PRIMARY KEY (session_id, placement_revision),
    CONSTRAINT runner_workspace_release_correlation_key
        UNIQUE (session_id, placement_revision, runner_id, manifest_id),
    CONSTRAINT runner_workspace_release_positive_u64 CHECK (
        placement_revision BETWEEN 1 AND 18446744073709551615
        AND retired_placement_event_ordinal BETWEEN 1 AND 18446744073709551615
        AND successor_placement_event_ordinal BETWEEN 1 AND 18446744073709551615
        AND connection_epoch BETWEEN 1 AND 18446744073709551615
        AND connection_event_ordinal BETWEEN 1 AND 18446744073709551615
    ),
    CONSTRAINT runner_workspace_release_successor CHECK (
        successor_placement_event_ordinal =
            retired_placement_event_ordinal + 1
        AND successor_placement_event_ordinal <= 18446744073709551615
    ),
    CONSTRAINT runner_workspace_release_state_closed
        CHECK (state_kind = 'pending'),
    CONSTRAINT runner_workspace_release_retired_placement_fk
        FOREIGN KEY (
            session_id,
            retired_placement_event_ordinal,
            placement_revision
        )
        REFERENCES runner_session_placement_record (
            session_id,
            event_ordinal,
            placement_revision
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT runner_workspace_release_successor_placement_fk
        FOREIGN KEY (session_id, successor_placement_event_ordinal)
        REFERENCES runner_session_placement_record (session_id, event_ordinal)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT runner_workspace_release_enrollment_fk
        FOREIGN KEY (enrollment_id)
        REFERENCES runner_enrollment (enrollment_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT runner_workspace_release_connection_fk
        FOREIGN KEY (
            enrollment_id,
            connection_epoch,
            connection_event_ordinal
        )
        REFERENCES runner_connection_event (
            enrollment_id,
            connection_epoch,
            event_ordinal
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE FUNCTION require_runner_workspace_release_authority()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    retired runner_session_placement_record%ROWTYPE;
    successor runner_session_placement_record%ROWTYPE;
    current_placement_ordinal numeric(20, 0);
    enrollment runner_enrollment%ROWTYPE;
    connection_head runner_connection_authority_head%ROWTYPE;
    connection runner_connection_event%ROWTYPE;
BEGIN
    SELECT * INTO retired
      FROM runner_session_placement_record
     WHERE session_id = NEW.session_id
       AND event_ordinal = NEW.retired_placement_event_ordinal;
    SELECT * INTO successor
      FROM runner_session_placement_record
     WHERE session_id = NEW.session_id
       AND event_ordinal = NEW.successor_placement_event_ordinal;
    SELECT event_ordinal INTO current_placement_ordinal
      FROM runner_current_session_placement
     WHERE session_id = NEW.session_id;
    SELECT * INTO enrollment
      FROM runner_enrollment
     WHERE enrollment_id = NEW.enrollment_id;
    SELECT * INTO connection_head
      FROM runner_connection_authority_head
     WHERE enrollment_id = NEW.enrollment_id;
    SELECT * INTO connection
      FROM runner_connection_event
     WHERE enrollment_id = NEW.enrollment_id
       AND connection_epoch = NEW.connection_epoch
       AND event_ordinal = NEW.connection_event_ordinal;

    IF NEW.state_kind <> 'pending'
       OR current_placement_ordinal IS DISTINCT FROM
            NEW.successor_placement_event_ordinal
       OR retired.event_kind IS DISTINCT FROM 'runner_lost'
       OR retired.state_kind IS DISTINCT FROM 'runner_lost'
       OR retired.placement_revision IS DISTINCT FROM NEW.placement_revision
       OR retired.loss_source_kind IS DISTINCT FROM 'registration'
       OR retired.lost_runner_id IS DISTINCT FROM NEW.runner_id
       OR retired.pinned_runner_id IS DISTINCT FROM NEW.runner_id
       OR retired.registration_enrollment_id IS DISTINCT FROM NEW.enrollment_id
       OR retired.workspace_manifest_id IS DISTINCT FROM NEW.manifest_id
       OR retired.workspace_placement_revision IS DISTINCT FROM
            NEW.placement_revision
       OR successor.event_kind IS DISTINCT FROM 'runner_replaced'
       OR successor.state_kind IS DISTINCT FROM 'pinned'
       OR successor.event_ordinal IS DISTINCT FROM
            NEW.successor_placement_event_ordinal
       OR successor.placement_revision IS DISTINCT FROM
            NEW.placement_revision + 1
       OR successor.pinned_runner_id IS DISTINCT FROM NEW.runner_id
       OR successor.registration_enrollment_id IS DISTINCT FROM
            NEW.enrollment_id
       OR successor.workspace_manifest_id IS NULL
       OR successor.workspace_manifest_id = NEW.manifest_id
       OR successor.workspace_placement_revision IS DISTINCT FROM
            successor.placement_revision
       OR enrollment.runner_id IS DISTINCT FROM NEW.runner_id
       OR enrollment.state_kind IS DISTINCT FROM 'active'
       OR connection_head.connection_epoch IS DISTINCT FROM
            NEW.connection_epoch
       OR connection_head.connection_event_ordinal IS DISTINCT FROM
            NEW.connection_event_ordinal
       OR connection.state_kind IS DISTINCT FROM 'connected'
       OR EXISTS (
            SELECT 1
              FROM runner_lease_generation AS generation
              JOIN runner_current_lease_event AS lease_head
                ON lease_head.lease_id = generation.lease_id
               AND lease_head.generation = generation.generation
              JOIN runner_lease_event AS lease_event
                ON lease_event.lease_id = lease_head.lease_id
               AND lease_event.generation = lease_head.generation
               AND lease_event.event_ordinal = lease_head.event_ordinal
             WHERE generation.session_id = NEW.session_id
               AND generation.placement_event_ordinal =
                    NEW.retired_placement_event_ordinal
               AND lease_event.state_kind IN (
                    'offered',
                    'claimed',
                    'lost_execution_possible',
                    'lost_claimed'
               )
       )
    THEN
        RAISE EXCEPTION 'workspace release lacks exact retired placement authority'
            USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER runner_workspace_release_is_checked
AFTER INSERT ON runner_workspace_release
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_runner_workspace_release_authority();

CREATE TRIGGER runner_workspace_release_is_append_only
BEFORE UPDATE OR DELETE ON runner_workspace_release
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER runner_workspace_release_rejects_truncate
BEFORE TRUNCATE ON runner_workspace_release
FOR EACH STATEMENT
EXECUTE FUNCTION reject_immutable_record_change();
