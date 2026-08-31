-- Retain one authenticated ready-workspace receipt for a staged repository
-- replacement. Wire admission and replacement terminalization follow later.

CREATE TABLE runner_replacement_workspace_receipt (
    authorization_id uuid PRIMARY KEY,
    session_id uuid NOT NULL,
    placement_revision numeric(20, 0) NOT NULL,
    runner_id uuid NOT NULL,
    manifest_id uuid NOT NULL UNIQUE,
    manifest_digest text NOT NULL,
    repository_key runner_catalog_name NOT NULL,
    canonical_clone_url_digest text NOT NULL,
    credential_profile_name runner_catalog_name,
    sandbox_profile text NOT NULL,
    working_directory runner_exact_text NOT NULL,
    relative_path runner_exact_text NOT NULL,
    recovery_kind text NOT NULL,
    branch_name text,
    revision text NOT NULL,

    CONSTRAINT runner_replacement_workspace_receipt_positive_revision CHECK (
        placement_revision BETWEEN 1 AND 18446744073709551615
    ),
    CONSTRAINT runner_replacement_workspace_receipt_digest_shape CHECK (
        manifest_digest ~ '^[0-9a-f]{64}$'
        AND canonical_clone_url_digest ~ '^[0-9a-f]{64}$'
    ),
    CONSTRAINT runner_replacement_workspace_receipt_sandbox_closed CHECK (
        sandbox_profile IN ('workspace_restricted', 'ambient')
    ),
    CONSTRAINT runner_replacement_workspace_receipt_relative_path_shape CHECK (
        relative_path !~ '(^/|//|(^|/)\.{1,2}(/|$))'
    ),
    CONSTRAINT runner_replacement_workspace_receipt_revision_shape CHECK (
        revision ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'
    ),
    CONSTRAINT runner_replacement_workspace_receipt_recovery_shape CHECK (
        (recovery_kind = 'commit' AND branch_name IS NULL)
        OR (
            recovery_kind = 'branch'
            AND branch_name IS NOT NULL
            AND octet_length(branch_name) BETWEEN 1 AND 255
            AND branch_name !~ '[[:cntrl:] ~^:?*]'
            AND position('[' IN branch_name) = 0
            AND position(chr(92) IN branch_name) = 0
            AND branch_name !~ '(^-|^/|/$|//|\.\.|@\{|\.$)'
            AND branch_name !~ '(^|/)\.'
            AND branch_name !~ '\.lock(?:/|$)'
            AND branch_name <> '@'
        )
    ),
    CONSTRAINT runner_replacement_workspace_receipt_authorization_fk
        FOREIGN KEY (authorization_id, session_id)
        REFERENCES runner_workspace_provisioning_authorization (
            authorization_id, session_id
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE FUNCTION require_runner_replacement_workspace_receipt_authority()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    staged runner_workspace_provisioning_authorization%ROWTYPE;
    command replace_lost_runner_command%ROWTYPE;
    enrollment runner_enrollment%ROWTYPE;
    connection_head runner_connection_authority_head%ROWTYPE;
    connection runner_connection_event%ROWTYPE;
    current_registration numeric(20, 0);
    current_placement_ordinal numeric(20, 0);
    placement runner_session_placement_record%ROWTYPE;
BEGIN
    SELECT * INTO staged
      FROM runner_workspace_provisioning_authorization
     WHERE authorization_id = NEW.authorization_id;
    SELECT * INTO command
      FROM replace_lost_runner_command
     WHERE command_id = staged.command_id
     -- Terminal-result insertion locks this command row, so this lock also
     -- serializes the absence check below with replacement terminalization.
     FOR UPDATE;
    SELECT * INTO enrollment
      FROM runner_enrollment
     WHERE enrollment_id = staged.enrollment_id
     FOR SHARE;
    SELECT * INTO connection_head
      FROM runner_connection_authority_head
     WHERE enrollment_id = staged.enrollment_id
     FOR SHARE;
    SELECT * INTO connection
      FROM runner_connection_event
     WHERE enrollment_id = connection_head.enrollment_id
       AND connection_epoch = connection_head.connection_epoch
       AND event_ordinal = connection_head.connection_event_ordinal;
    SELECT registration_revision INTO current_registration
      FROM runner_current_registration
     WHERE enrollment_id = staged.enrollment_id
     FOR SHARE;
    SELECT event_ordinal INTO current_placement_ordinal
      FROM runner_current_session_placement
     WHERE session_id = NEW.session_id
     FOR SHARE;
    SELECT * INTO placement
      FROM runner_session_placement_record
     WHERE session_id = NEW.session_id
       AND event_ordinal = current_placement_ordinal;

    IF staged.session_id IS DISTINCT FROM NEW.session_id
       OR staged.successor_placement_revision IS DISTINCT FROM
            NEW.placement_revision
       OR staged.runner_id IS DISTINCT FROM NEW.runner_id
       OR staged.repository_key IS DISTINCT FROM NEW.repository_key
       OR staged.credential_profile_name IS DISTINCT FROM
            NEW.credential_profile_name
       OR staged.sandbox_profile IS DISTINCT FROM NEW.sandbox_profile
       OR NEW.relative_path IS DISTINCT FROM format(
            'sessions/%s/%s/repo',
            NEW.session_id::text,
            NEW.placement_revision::text
       )
       OR current_placement_ordinal IS DISTINCT FROM
            staged.lost_placement_event_ordinal
       OR placement.placement_revision IS DISTINCT FROM
            staged.lost_placement_revision
       OR placement.state_kind IS DISTINCT FROM 'runner_lost'
       OR placement.workspace_manifest_id IS NOT DISTINCT FROM NEW.manifest_id
       OR enrollment.runner_id IS DISTINCT FROM NEW.runner_id
       OR connection_head.connection_epoch IS DISTINCT FROM
            staged.connection_epoch
       OR connection_head.connection_event_ordinal IS DISTINCT FROM
            staged.connection_event_ordinal
       OR connection.state_kind IS DISTINCT FROM 'connected'
       OR current_registration IS DISTINCT FROM
            staged.registration_revision
       OR EXISTS (
            SELECT 1
              FROM replace_lost_runner_result AS result
             WHERE result.command_id = staged.command_id
       )
       OR (
            command.target_kind IN ('runner', 'same_runner_reenrollment')
            AND enrollment.state_kind IS DISTINCT FROM 'active'
       )
       OR (
            command.target_kind = 'pending_enrollment'
            AND (
                enrollment.state_kind IS DISTINCT FROM 'pending'
                OR NOT EXISTS (
                    SELECT 1
                      FROM runner_pending_enrollment AS pending
                     WHERE pending.request_id =
                            command.target_pending_request_id
                       AND pending.enrollment_id = staged.enrollment_id
                )
            )
       )
    THEN
        RAISE EXCEPTION 'workspace receipt lacks exact current provisioning authority'
            USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER runner_replacement_workspace_receipt_is_checked
AFTER INSERT ON runner_replacement_workspace_receipt
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_runner_replacement_workspace_receipt_authority();

CREATE TRIGGER runner_replacement_workspace_receipt_is_append_only
BEFORE UPDATE OR DELETE ON runner_replacement_workspace_receipt
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER runner_replacement_workspace_receipt_rejects_truncate
BEFORE TRUNCATE ON runner_replacement_workspace_receipt
FOR EACH STATEMENT
EXECUTE FUNCTION reject_immutable_record_change();
