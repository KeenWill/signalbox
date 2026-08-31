-- Version-two workspace-ready receipts retain the runner-authored absolute
-- execution directory. Existing version-one receipts cannot be upgraded
-- without guessing the runner root, so this migration intentionally refuses a
-- populated legacy relation.

ALTER TABLE runner_replacement_workspace_receipt
    ADD COLUMN execution_directory runner_exact_text NOT NULL,
    ADD CONSTRAINT runner_replacement_workspace_receipt_execution_directory_shape
        CHECK (execution_directory LIKE '/%');

CREATE OR REPLACE FUNCTION require_runner_replacement_workspace_receipt_authority()
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
     WHERE command_id = staged.command_id;
    SELECT * INTO enrollment
      FROM runner_enrollment
     WHERE enrollment_id = staged.enrollment_id;
    SELECT * INTO connection_head
      FROM runner_connection_authority_head
     WHERE enrollment_id = staged.enrollment_id;
    SELECT * INTO connection
      FROM runner_connection_event
     WHERE enrollment_id = connection_head.enrollment_id
       AND connection_epoch = connection_head.connection_epoch
       AND event_ordinal = connection_head.connection_event_ordinal;
    SELECT registration_revision INTO current_registration
      FROM runner_current_registration
     WHERE enrollment_id = staged.enrollment_id;
    SELECT event_ordinal INTO current_placement_ordinal
      FROM runner_current_session_placement
     WHERE session_id = NEW.session_id;
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
       OR NEW.execution_directory NOT LIKE '/%'
       OR current_placement_ordinal IS DISTINCT FROM
            staged.lost_placement_event_ordinal
       OR placement.placement_revision IS DISTINCT FROM
            staged.lost_placement_revision
       OR placement.state_kind IS DISTINCT FROM 'runner_lost'
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
