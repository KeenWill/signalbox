-- Activate one provisioning-only pending enrollment in the same terminal
-- transaction that installs its predecessor's workspace-free pinned successor.

CREATE OR REPLACE FUNCTION require_pinned_replace_lost_runner_result_authority()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    request replace_lost_runner_command%ROWTYPE;
    stage runner_workspace_free_replacement_stage%ROWTYPE;
    current_placement runner_session_placement_record%ROWTYPE;
    lost_placement runner_session_placement_record%ROWTYPE;
    enrollment runner_enrollment%ROWTYPE;
    registration runner_registration%ROWTYPE;
    loss_registration runner_registration%ROWTYPE;
    pending runner_pending_enrollment%ROWTYPE;
    predecessor runner_enrollment%ROWTYPE;
    predecessor_loss runner_connection_loss_epoch%ROWTYPE;
    current_registration numeric(20, 0);
    connection_head runner_connection_authority_head%ROWTYPE;
    connection runner_connection_event%ROWTYPE;
    boundary_count bigint;
BEGIN
    SELECT * INTO request
      FROM replace_lost_runner_command
     WHERE command_id = NEW.command_id;
    SELECT * INTO stage
      FROM runner_workspace_free_replacement_stage
     WHERE command_id = NEW.command_id;
    SELECT placement.* INTO current_placement
      FROM runner_current_session_placement AS current_head
      JOIN runner_session_placement_record AS placement
        ON placement.session_id = current_head.session_id
       AND placement.event_ordinal = current_head.event_ordinal
     WHERE current_head.session_id = NEW.session_id;
    SELECT * INTO lost_placement
      FROM runner_session_placement_record
     WHERE session_id = NEW.session_id
       AND event_ordinal = stage.lost_placement_event_ordinal;
    SELECT * INTO enrollment
      FROM runner_enrollment
     WHERE enrollment_id = NEW.target_enrollment_id;
    SELECT * INTO registration
      FROM runner_registration
     WHERE enrollment_id = NEW.target_enrollment_id
       AND registration_revision = NEW.target_registration_revision;
    SELECT * INTO loss_registration
      FROM runner_registration
     WHERE enrollment_id = lost_placement.registration_enrollment_id
       AND registration_revision = lost_placement.loss_registration_revision;
    SELECT * INTO pending
      FROM runner_pending_enrollment
     WHERE request_id = request.target_pending_request_id;
    SELECT * INTO predecessor
      FROM runner_enrollment
     WHERE enrollment_id = pending.predecessor_enrollment_id;
    SELECT * INTO predecessor_loss
      FROM runner_connection_loss_epoch
     WHERE enrollment_id = pending.predecessor_enrollment_id
       AND loss_epoch = pending.predecessor_loss_epoch;
    SELECT registration_revision INTO current_registration
      FROM runner_current_registration
     WHERE enrollment_id = NEW.target_enrollment_id;
    SELECT * INTO connection_head
      FROM runner_connection_authority_head
     WHERE enrollment_id = NEW.target_enrollment_id;
    SELECT * INTO connection
      FROM runner_connection_event
     WHERE enrollment_id = connection_head.enrollment_id
       AND connection_epoch = connection_head.connection_epoch
       AND event_ordinal = connection_head.connection_event_ordinal;
    SELECT count(*) INTO boundary_count
      FROM session_runner_placement_frontier AS pointer
      JOIN semantic_transcript_entry AS entry
        ON entry.source_session_id = pointer.session_id
       AND entry.semantic_entry_id = pointer.semantic_entry_id
       AND entry.payload_kind = 'runner_placement_changed'
       AND entry.runner_placement_revision = pointer.placement_revision
       AND entry.runner_placement_event_ordinal = NEW.placement_event_ordinal
     WHERE pointer.session_id = NEW.session_id
       AND pointer.placement_revision = NEW.placement_revision;

    IF request.command_id IS NULL
       OR request.session_id IS DISTINCT FROM NEW.session_id
       OR NOT (
            (request.target_kind = 'runner'
                AND request.target_runner_id IS NOT DISTINCT FROM
                        NEW.new_runner_id
                AND NEW.prior_runner_id <> NEW.new_runner_id)
            OR (request.target_kind = 'same_runner_reenrollment'
                AND request.target_runner_id IS NOT DISTINCT FROM
                        NEW.new_runner_id
                AND NEW.prior_runner_id = NEW.new_runner_id
                AND lost_placement.loss_source_kind = 'registration'
                AND lost_placement.loss_registration_revision IS NOT NULL
                AND lost_placement.registration_enrollment_id IS NOT DISTINCT FROM
                        NEW.target_enrollment_id
                AND loss_registration.enrollment_id IS NOT NULL
                AND loss_registration.enrollment_id IS NOT DISTINCT FROM
                        NEW.target_enrollment_id
                AND loss_registration.runner_id IS NOT DISTINCT FROM
                        NEW.new_runner_id
                AND loss_registration.authentication_reference_id IS NOT DISTINCT FROM
                        registration.authentication_reference_id
                AND lost_placement.loss_registration_revision <=
                        NEW.target_registration_revision)
            OR (request.target_kind = 'pending_enrollment'
                AND request.target_pending_request_id IS NOT NULL
                AND pending.request_id IS NOT NULL
                AND pending.enrollment_id IS NOT DISTINCT FROM
                        NEW.target_enrollment_id
                AND pending.predecessor_enrollment_id IS NOT DISTINCT FROM
                        lost_placement.loss_fence_enrollment_id
                AND (lost_placement.observed_runner_loss_epoch IS NULL
                    OR lost_placement.observed_runner_loss_epoch <
                        pending.predecessor_loss_epoch)
                AND pending.predecessor_enrollment_id IS NOT DISTINCT FROM
                        predecessor.enrollment_id
                AND predecessor.runner_id IS NOT DISTINCT FROM
                        NEW.prior_runner_id
                AND predecessor.state_kind = 'revoked'
                AND predecessor_loss.enrollment_id IS NOT NULL
                AND predecessor_loss.enrollment_id IS NOT DISTINCT FROM
                        pending.predecessor_enrollment_id
                AND predecessor_loss.loss_epoch IS NOT DISTINCT FROM
                        pending.predecessor_loss_epoch
                AND lost_placement.loss_source_kind = 'connection'
                AND NEW.prior_runner_id <> NEW.new_runner_id)
       )
       OR request.expected_placement_revision IS DISTINCT FROM
            stage.lost_placement_revision
       OR stage.session_id IS DISTINCT FROM NEW.session_id
       OR stage.command_id IS DISTINCT FROM NEW.command_id
       OR stage.lost_placement_event_ordinal + 1 IS DISTINCT FROM
            NEW.placement_event_ordinal
       OR stage.requested_working_directory IS DISTINCT FROM
            NEW.working_directory
       OR lost_placement.event_kind IS DISTINCT FROM 'runner_lost'
       OR lost_placement.state_kind IS DISTINCT FROM 'runner_lost'
       OR lost_placement.placement_revision IS DISTINCT FROM
            stage.lost_placement_revision
       OR lost_placement.lost_runner_id IS DISTINCT FROM NEW.prior_runner_id
       OR current_placement.event_ordinal IS DISTINCT FROM
            NEW.placement_event_ordinal
       OR current_placement.placement_revision IS DISTINCT FROM
            NEW.placement_revision
       OR current_placement.placement_revision IS DISTINCT FROM
            stage.lost_placement_revision + 1
       OR current_placement.event_kind IS DISTINCT FROM 'runner_replaced'
       OR current_placement.state_kind IS DISTINCT FROM 'pinned'
       OR current_placement.selector_kind IS DISTINCT FROM
            lost_placement.selector_kind
       OR current_placement.selector_runner_id IS DISTINCT FROM
            lost_placement.selector_runner_id
       OR current_placement.selector_capability_class IS DISTINCT FROM
            lost_placement.selector_capability_class
       OR current_placement.pinned_runner_id IS DISTINCT FROM
            NEW.new_runner_id
       OR current_placement.registration_enrollment_id IS DISTINCT FROM
            NEW.target_enrollment_id
       OR current_placement.registration_revision IS DISTINCT FROM
            NEW.target_registration_revision
       OR current_placement.workspace_requirement_kind IS DISTINCT FROM 'none'
       OR current_placement.directory_selection_kind IS DISTINCT FROM 'exact'
       OR current_placement.directory_selection_kind IS DISTINCT FROM
            lost_placement.directory_selection_kind
       OR current_placement.requested_working_directory IS DISTINCT FROM
            NEW.working_directory
       OR current_placement.requested_working_directory IS DISTINCT FROM
            lost_placement.requested_working_directory
       OR current_placement.pinned_working_directory IS DISTINCT FROM
            NEW.working_directory
       OR current_placement.requested_sandbox_profile IS DISTINCT FROM
            NEW.sandbox_profile
       OR current_placement.requested_sandbox_profile IS DISTINCT FROM
            lost_placement.requested_sandbox_profile
       OR current_placement.requested_credential_profile_name IS NOT NULL
       OR current_placement.pinned_credential_profile_name IS NOT NULL
       OR lost_placement.requested_credential_profile_name IS NOT NULL
       OR current_placement.requested_repository_key IS DISTINCT FROM
            lost_placement.requested_repository_key
       OR current_placement.permission_override_count IS DISTINCT FROM
            lost_placement.permission_override_count
       OR enrollment.runner_id IS DISTINCT FROM NEW.new_runner_id
       OR enrollment.state_kind IS DISTINCT FROM 'active'
       OR registration.runner_id IS DISTINCT FROM NEW.new_runner_id
       OR current_registration IS DISTINCT FROM
            NEW.target_registration_revision
       OR connection_head.connection_epoch IS DISTINCT FROM
            NEW.target_connection_epoch
       OR connection_head.connection_event_ordinal IS DISTINCT FROM
            NEW.target_connection_event_ordinal
       OR connection.state_kind IS DISTINCT FROM 'connected'
       OR NOT EXISTS (
            SELECT 1
              FROM runner_registration_sandbox AS sandbox
             WHERE sandbox.enrollment_id = NEW.target_enrollment_id
               AND sandbox.registration_revision =
                    NEW.target_registration_revision
               AND sandbox.sandbox_profile = NEW.sandbox_profile
       )
       OR NOT (
            (current_placement.selector_kind = 'identity'
                AND current_placement.selector_runner_id = NEW.new_runner_id)
            OR (current_placement.selector_kind = 'capability_class'
                AND EXISTS (
                    SELECT 1
                      FROM runner_registration_class AS class
                     WHERE class.enrollment_id = NEW.target_enrollment_id
                       AND class.registration_revision =
                            NEW.target_registration_revision
                       AND class.capability_class =
                            current_placement.selector_capability_class
                ))
       )
       OR EXISTS (
            SELECT 1
              FROM runner_session_placement_permission_override AS permission_override
             WHERE permission_override.session_id = current_placement.session_id
               AND permission_override.event_ordinal =
                    current_placement.event_ordinal
               AND NOT EXISTS (
                    SELECT 1
                      FROM runner_registration_tool AS tool
                     WHERE tool.enrollment_id = NEW.target_enrollment_id
                       AND tool.registration_revision =
                            NEW.target_registration_revision
                       AND tool.tool_name = permission_override.tool_name
               )
       )
       OR EXISTS (
            SELECT 1
              FROM runner_session_placement_permission_override AS prior_override
              LEFT JOIN runner_session_placement_permission_override AS successor_override
                ON successor_override.session_id = current_placement.session_id
               AND successor_override.event_ordinal =
                    current_placement.event_ordinal
               AND successor_override.tool_name = prior_override.tool_name
               AND successor_override.permission_kind =
                    prior_override.permission_kind
             WHERE prior_override.session_id = lost_placement.session_id
               AND prior_override.event_ordinal = lost_placement.event_ordinal
               AND successor_override.tool_name IS NULL
       )
       OR EXISTS (
            SELECT 1
              FROM runner_session_placement_permission_override AS successor_override
              LEFT JOIN runner_session_placement_permission_override AS prior_override
                ON prior_override.session_id = lost_placement.session_id
               AND prior_override.event_ordinal = lost_placement.event_ordinal
               AND prior_override.tool_name = successor_override.tool_name
               AND prior_override.permission_kind =
                    successor_override.permission_kind
             WHERE successor_override.session_id = current_placement.session_id
               AND successor_override.event_ordinal =
                    current_placement.event_ordinal
               AND prior_override.tool_name IS NULL
       )
       OR boundary_count <> 1
       OR EXISTS (
            SELECT 1
              FROM runner_workspace_provisioning_authorization AS provisioning
             WHERE provisioning.command_id = NEW.command_id
       )
    THEN
        RAISE EXCEPTION 'applied pinned replacement lacks exact terminal authority'
            USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;
