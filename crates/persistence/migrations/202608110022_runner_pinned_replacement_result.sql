-- Admit the terminal receipt for one workspace-free pinned replacement. The
-- existing pre-pin and refusal arms retain their exact prior shapes.

ALTER TABLE replace_lost_runner_result
    ADD COLUMN working_directory runner_exact_text,
    DROP CONSTRAINT replace_lost_runner_result_shape,
    ADD CONSTRAINT replace_lost_runner_result_shape CHECK (
        (result_kind = 'applied'
            AND rejection_kind IS NULL
            AND target_unavailable_reason IS NULL
            AND placement_event_ordinal IS NOT NULL
            AND placement_revision IS NOT NULL
            AND placement_state_kind = 'unpinned'
            AND prior_runner_id IS NOT NULL
            AND new_runner_id IS NOT NULL
            AND prior_runner_id <> new_runner_id
            AND sandbox_profile IS NOT NULL
            AND working_directory IS NULL)
        OR (result_kind = 'applied'
            AND rejection_kind IS NULL
            AND target_unavailable_reason IS NULL
            AND placement_event_ordinal IS NOT NULL
            AND placement_revision IS NOT NULL
            AND placement_state_kind = 'pinned'
            AND prior_runner_id IS NOT NULL
            AND new_runner_id IS NOT NULL
            AND prior_runner_id <> new_runner_id
            AND sandbox_profile IS NOT NULL
            AND working_directory IS NOT NULL)
        OR (result_kind = 'rejected'
            AND rejection_kind IN ('session_not_found', 'runner_placement_not_found')
            AND target_unavailable_reason IS NULL
            AND placement_event_ordinal IS NULL
            AND placement_revision IS NULL
            AND placement_state_kind IS NULL
            AND prior_runner_id IS NULL
            AND new_runner_id IS NULL
            AND sandbox_profile IS NULL
            AND working_directory IS NULL)
        OR (result_kind = 'rejected'
            AND rejection_kind = 'placement_revision_mismatch'
            AND target_unavailable_reason IS NULL
            AND placement_event_ordinal IS NOT NULL
            AND placement_revision IS NOT NULL
            AND placement_state_kind IS NOT NULL
            AND prior_runner_id IS NULL
            AND new_runner_id IS NULL
            AND sandbox_profile IS NULL
            AND working_directory IS NULL)
        OR (result_kind = 'rejected'
            AND rejection_kind = 'placement_not_lost'
            AND target_unavailable_reason IS NULL
            AND placement_event_ordinal IS NOT NULL
            AND placement_revision IS NOT NULL
            AND placement_state_kind IN ('unpinned', 'pinned', 'runner_abandoned')
            AND prior_runner_id IS NULL
            AND new_runner_id IS NULL
            AND sandbox_profile IS NULL
            AND working_directory IS NULL)
        OR (result_kind = 'rejected'
            AND rejection_kind = 'replacement_same_runner'
            AND target_unavailable_reason IS NULL
            AND placement_event_ordinal IS NOT NULL
            AND placement_revision IS NOT NULL
            AND placement_state_kind IN ('runner_lost_before_pin', 'runner_lost')
            AND prior_runner_id IS NOT NULL
            AND new_runner_id = prior_runner_id
            AND sandbox_profile IS NULL
            AND working_directory IS NULL)
        OR (result_kind = 'rejected'
            AND rejection_kind = 'replacement_target_unavailable'
            AND target_unavailable_reason IS NOT NULL
            AND placement_event_ordinal IS NOT NULL
            AND placement_revision IS NOT NULL
            AND placement_state_kind IN ('runner_lost_before_pin', 'runner_lost')
            AND prior_runner_id IS NOT NULL
            AND (
                (target_unavailable_reason = 'pending_request_mismatch'
                    AND (new_runner_id IS NULL OR new_runner_id <> prior_runner_id))
                OR (target_unavailable_reason <> 'pending_request_mismatch'
                    AND new_runner_id IS NOT NULL)
            )
            AND sandbox_profile IS NULL
            AND working_directory IS NULL)
    );

DROP TRIGGER replace_lost_runner_result_is_authorized
    ON replace_lost_runner_result;

CREATE CONSTRAINT TRIGGER replace_lost_runner_result_is_authorized
AFTER INSERT ON replace_lost_runner_result
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
WHEN (NOT (
    NEW.result_kind = 'applied'
    AND NEW.placement_state_kind = 'pinned'
))
EXECUTE FUNCTION require_replace_lost_runner_result_authority();

CREATE FUNCTION require_pinned_replace_lost_runner_result_authority()
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
       OR request.target_kind IS DISTINCT FROM 'runner'
       OR request.target_runner_id IS DISTINCT FROM NEW.new_runner_id
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

CREATE CONSTRAINT TRIGGER pinned_replace_lost_runner_result_is_authorized
AFTER INSERT ON replace_lost_runner_result
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
WHEN (
    NEW.result_kind = 'applied'
    AND NEW.placement_state_kind = 'pinned'
)
EXECUTE FUNCTION require_pinned_replace_lost_runner_result_authority();
