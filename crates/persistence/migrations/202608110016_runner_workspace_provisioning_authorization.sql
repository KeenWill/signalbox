-- Retain one checked repository-provisioning boundary for a staged pinned
-- runner replacement. The producer transaction follows in a later slice.

CREATE TABLE runner_workspace_provisioning_authorization (
    authorization_id uuid PRIMARY KEY,
    command_id uuid NOT NULL UNIQUE,
    session_id uuid NOT NULL,
    lost_placement_event_ordinal numeric(20, 0) NOT NULL,
    lost_placement_revision numeric(20, 0) NOT NULL,
    successor_placement_revision numeric(20, 0) NOT NULL,
    enrollment_id uuid NOT NULL,
    runner_id uuid NOT NULL,
    registration_revision numeric(20, 0) NOT NULL,
    connection_epoch numeric(20, 0) NOT NULL,
    connection_event_ordinal numeric(20, 0) NOT NULL,
    repository_key runner_catalog_name NOT NULL,
    sandbox_profile text NOT NULL,
    credential_profile_name runner_catalog_name,

    CONSTRAINT runner_workspace_provisioning_authorization_identity
        UNIQUE (authorization_id, session_id),
    CONSTRAINT runner_workspace_provisioning_authorization_command_session
        UNIQUE (command_id, session_id),
    CONSTRAINT runner_workspace_provisioning_authorization_positive_u64 CHECK (
        lost_placement_event_ordinal BETWEEN 1 AND 18446744073709551615
        AND lost_placement_revision BETWEEN 1 AND 18446744073709551615
        AND successor_placement_revision BETWEEN 1 AND 18446744073709551615
        AND registration_revision BETWEEN 1 AND 18446744073709551615
        AND connection_epoch BETWEEN 1 AND 18446744073709551615
        AND connection_event_ordinal BETWEEN 1 AND 18446744073709551615
    ),
    CONSTRAINT runner_workspace_provisioning_authorization_successor CHECK (
        successor_placement_revision = lost_placement_revision + 1
        AND successor_placement_revision <= 18446744073709551615
    ),
    CONSTRAINT runner_workspace_provisioning_authorization_sandbox_closed
        CHECK (sandbox_profile IN ('workspace_restricted', 'ambient')),
    CONSTRAINT runner_workspace_provisioning_authorization_command_fk
        FOREIGN KEY (command_id, session_id)
        REFERENCES replace_lost_runner_command (command_id, session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT runner_workspace_provisioning_authorization_placement_fk
        FOREIGN KEY (
            session_id, lost_placement_event_ordinal,
            lost_placement_revision
        )
        REFERENCES runner_session_placement_record (
            session_id, event_ordinal, placement_revision
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT runner_workspace_provisioning_authorization_registration_fk
        FOREIGN KEY (enrollment_id, registration_revision, runner_id)
        REFERENCES runner_registration (
            enrollment_id, registration_revision, runner_id
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT runner_workspace_provisioning_authorization_connection_fk
        FOREIGN KEY (
            enrollment_id, connection_epoch, connection_event_ordinal
        )
        REFERENCES runner_connection_event (
            enrollment_id, connection_epoch, event_ordinal
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT runner_workspace_provisioning_authorization_sandbox_fk
        FOREIGN KEY (
            enrollment_id, registration_revision, sandbox_profile
        )
        REFERENCES runner_registration_sandbox (
            enrollment_id, registration_revision, sandbox_profile
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT runner_workspace_provisioning_authorization_profile_fk
        FOREIGN KEY (
            enrollment_id, registration_revision, credential_profile_name
        )
        REFERENCES runner_registration_profile (
            enrollment_id, registration_revision, credential_profile_name
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE FUNCTION require_runner_workspace_provisioning_authorization()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    request replace_lost_runner_command%ROWTYPE;
    enrollment runner_enrollment%ROWTYPE;
    connection_head runner_connection_authority_head%ROWTYPE;
    connection runner_connection_event%ROWTYPE;
    current_registration numeric(20, 0);
    placement runner_session_placement_record%ROWTYPE;
    current_placement_ordinal numeric(20, 0);
    pending_request uuid;
    repository_profile text;
BEGIN
    SELECT * INTO request
      FROM replace_lost_runner_command
     WHERE command_id = NEW.command_id;
    IF NOT FOUND OR request.session_id <> NEW.session_id THEN
        RAISE EXCEPTION 'workspace authorization lacks its exact replacement command'
            USING ERRCODE = '23514';
    END IF;

    -- The future producer takes the session scheduler first. These relational
    -- checks then follow the runner lock order used by every replacement.
    SELECT * INTO enrollment
      FROM runner_enrollment
     WHERE enrollment_id = NEW.enrollment_id
     FOR SHARE;
    SELECT * INTO connection_head
      FROM runner_connection_authority_head
     WHERE enrollment_id = NEW.enrollment_id
     FOR SHARE;
    SELECT * INTO connection
      FROM runner_connection_event
     WHERE enrollment_id = connection_head.enrollment_id
       AND connection_epoch = connection_head.connection_epoch
       AND event_ordinal = connection_head.connection_event_ordinal;
    SELECT registration_revision INTO current_registration
      FROM runner_current_registration
     WHERE enrollment_id = NEW.enrollment_id
     FOR SHARE;
    SELECT event_ordinal INTO current_placement_ordinal
      FROM runner_current_session_placement
     WHERE session_id = NEW.session_id
     FOR SHARE;
    SELECT * INTO placement
      FROM runner_session_placement_record
     WHERE session_id = NEW.session_id
       AND event_ordinal = current_placement_ordinal;

    IF enrollment.runner_id IS DISTINCT FROM NEW.runner_id
       OR connection_head.connection_epoch IS DISTINCT FROM NEW.connection_epoch
       OR connection_head.connection_event_ordinal IS DISTINCT FROM
            NEW.connection_event_ordinal
       OR connection.state_kind IS DISTINCT FROM 'connected'
       OR current_registration IS DISTINCT FROM NEW.registration_revision
       OR current_placement_ordinal IS DISTINCT FROM
            NEW.lost_placement_event_ordinal
       OR placement.placement_revision IS DISTINCT FROM
            NEW.lost_placement_revision
       OR placement.state_kind IS DISTINCT FROM 'runner_lost'
       OR request.expected_placement_revision IS DISTINCT FROM
            NEW.lost_placement_revision
       OR placement.workspace_requirement_kind IS DISTINCT FROM
            'repository_worktree'
       OR placement.requested_repository_key IS DISTINCT FROM
            NEW.repository_key
       OR placement.requested_sandbox_profile IS DISTINCT FROM
            NEW.sandbox_profile
       OR placement.requested_credential_profile_name IS DISTINCT FROM
            NEW.credential_profile_name
       OR EXISTS (
            SELECT 1
              FROM runner_session_placement_permission_override AS permission_override
             WHERE permission_override.session_id = placement.session_id
               AND permission_override.event_ordinal = placement.event_ordinal
               AND NOT EXISTS (
                    SELECT 1
                      FROM runner_registration_tool AS tool
                     WHERE tool.enrollment_id = NEW.enrollment_id
                       AND tool.registration_revision =
                            NEW.registration_revision
                       AND tool.tool_name = permission_override.tool_name
               )
       )
    THEN
        RAISE EXCEPTION 'workspace authorization lacks exact current placement authority'
            USING ERRCODE = '23514';
    END IF;

    IF request.target_kind IN ('runner', 'same_runner_reenrollment') THEN
        IF request.target_runner_id IS DISTINCT FROM NEW.runner_id
           OR enrollment.state_kind IS DISTINCT FROM 'active'
        THEN
            RAISE EXCEPTION 'workspace authorization names a non-current runner target'
                USING ERRCODE = '23514';
        END IF;
    ELSIF request.target_kind = 'pending_enrollment' THEN
        SELECT pending.request_id INTO pending_request
          FROM runner_pending_enrollment AS pending
         WHERE pending.request_id = request.target_pending_request_id
           AND pending.enrollment_id = NEW.enrollment_id;
        IF pending_request IS NULL
           OR enrollment.state_kind IS DISTINCT FROM 'pending'
        THEN
            RAISE EXCEPTION 'workspace authorization names a non-current pending target'
                USING ERRCODE = '23514';
        END IF;
    ELSE
        RAISE EXCEPTION 'workspace authorization names an unknown target kind'
            USING ERRCODE = '23514';
    END IF;

    IF request.target_kind = 'same_runner_reenrollment' THEN
        IF placement.loss_source_kind IS DISTINCT FROM 'registration'
           OR placement.lost_runner_id IS DISTINCT FROM NEW.runner_id
           OR placement.loss_registration_revision IS NULL
           OR placement.loss_registration_revision > NEW.registration_revision
        THEN
            RAISE EXCEPTION 'same-runner authorization lacks registration-loss lineage'
                USING ERRCODE = '23514';
        END IF;
    ELSIF placement.lost_runner_id = NEW.runner_id THEN
        RAISE EXCEPTION 'ordinary replacement authorization reuses the lost runner'
            USING ERRCODE = '23514';
    END IF;

    SELECT repository.credential_profile_name INTO repository_profile
      FROM runner_registration_repository AS repository
     WHERE repository.enrollment_id = NEW.enrollment_id
       AND repository.registration_revision = NEW.registration_revision
       AND repository.repository_key = NEW.repository_key;
    IF NOT FOUND OR repository_profile IS DISTINCT FROM NEW.credential_profile_name
       OR NOT EXISTS (
            SELECT 1
              FROM runner_registration_workspace AS workspace
             WHERE workspace.enrollment_id = NEW.enrollment_id
               AND workspace.registration_revision = NEW.registration_revision
               AND workspace.workspace_kind = 'worktree_per_session'
       )
    THEN
        RAISE EXCEPTION 'workspace authorization exceeds registered repository capability'
            USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER runner_workspace_provisioning_authorization_is_checked
AFTER INSERT ON runner_workspace_provisioning_authorization
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_runner_workspace_provisioning_authorization();

CREATE TRIGGER runner_workspace_provisioning_authorization_is_append_only
BEFORE UPDATE OR DELETE ON runner_workspace_provisioning_authorization
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER runner_workspace_provisioning_authorization_rejects_truncate
BEFORE TRUNCATE ON runner_workspace_provisioning_authorization
FOR EACH STATEMENT
EXECUTE FUNCTION reject_immutable_record_change();
