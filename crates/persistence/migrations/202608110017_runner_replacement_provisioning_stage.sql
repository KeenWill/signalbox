-- Generalize durable replacement refusals from pre-pin loss to pinned loss.
-- Successful pinned repository replacements remain nonterminal while their
-- workspace authorization is outstanding.

ALTER TABLE replace_lost_runner_result
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
            AND sandbox_profile IS NOT NULL)
        OR (result_kind = 'rejected'
            AND rejection_kind IN ('session_not_found', 'runner_placement_not_found')
            AND target_unavailable_reason IS NULL
            AND placement_event_ordinal IS NULL
            AND placement_revision IS NULL
            AND placement_state_kind IS NULL
            AND prior_runner_id IS NULL
            AND new_runner_id IS NULL
            AND sandbox_profile IS NULL)
        OR (result_kind = 'rejected'
            AND rejection_kind = 'placement_revision_mismatch'
            AND target_unavailable_reason IS NULL
            AND placement_event_ordinal IS NOT NULL
            AND placement_revision IS NOT NULL
            AND placement_state_kind IS NOT NULL
            AND prior_runner_id IS NULL
            AND new_runner_id IS NULL
            AND sandbox_profile IS NULL)
        OR (result_kind = 'rejected'
            AND rejection_kind = 'placement_not_lost'
            AND target_unavailable_reason IS NULL
            AND placement_event_ordinal IS NOT NULL
            AND placement_revision IS NOT NULL
            AND placement_state_kind IN ('unpinned', 'pinned', 'runner_abandoned')
            AND prior_runner_id IS NULL
            AND new_runner_id IS NULL
            AND sandbox_profile IS NULL)
        OR (result_kind = 'rejected'
            AND rejection_kind = 'replacement_same_runner'
            AND target_unavailable_reason IS NULL
            AND placement_event_ordinal IS NOT NULL
            AND placement_revision IS NOT NULL
            AND placement_state_kind IN ('runner_lost_before_pin', 'runner_lost')
            AND prior_runner_id IS NOT NULL
            AND new_runner_id = prior_runner_id
            AND sandbox_profile IS NULL)
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
            AND sandbox_profile IS NULL)
    );

CREATE OR REPLACE FUNCTION require_replace_lost_runner_result_authority()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    request replace_lost_runner_command%ROWTYPE;
    current_placement runner_session_placement_record%ROWTYPE;
    prior_placement runner_session_placement_record%ROWTYPE;
    session_exists boolean;
    target_enrollment uuid;
    target_runner uuid;
    target_registration numeric(20, 0);
    target_connection_epoch numeric(20, 0);
    target_connection_event_ordinal numeric(20, 0);
    target_connection_state text;
    target_predecessor_runner uuid;
    target_candidate_state text;
    target_predecessor_state text;
    target_is_advertised boolean;
BEGIN
    SELECT * INTO request
      FROM replace_lost_runner_command
     WHERE command_id = NEW.command_id;
    IF NOT FOUND OR request.session_id <> NEW.session_id THEN
        RAISE EXCEPTION 'runner replacement result lacks its exact request'
            USING ERRCODE = '23514';
    END IF;

    SELECT EXISTS (
        SELECT 1 FROM session WHERE session_id = request.session_id
    ) INTO session_exists;
    SELECT placement.* INTO current_placement
      FROM runner_current_session_placement AS current_head
      JOIN runner_session_placement_record AS placement
        ON placement.session_id = current_head.session_id
       AND placement.event_ordinal = current_head.event_ordinal
     WHERE current_head.session_id = request.session_id;

    IF request.target_kind = 'pending_enrollment' THEN
        SELECT candidate.enrollment_id, candidate.runner_id,
               candidate.state_kind, predecessor.runner_id,
               predecessor.state_kind
          INTO target_enrollment, target_runner, target_candidate_state,
               target_predecessor_runner, target_predecessor_state
          FROM runner_pending_enrollment AS pending
          JOIN runner_enrollment AS candidate
            ON candidate.enrollment_id = pending.enrollment_id
          JOIN runner_enrollment AS predecessor
            ON predecessor.enrollment_id = pending.predecessor_enrollment_id
         WHERE pending.request_id = request.target_pending_request_id;
    END IF;

    IF NEW.result_kind = 'applied' THEN
        SELECT * INTO prior_placement
          FROM runner_session_placement_record
         WHERE session_id = request.session_id
           AND event_ordinal = NEW.placement_event_ordinal - 1;
        IF NOT session_exists
           OR current_placement.event_ordinal IS DISTINCT FROM NEW.placement_event_ordinal
           OR current_placement.placement_revision IS DISTINCT FROM NEW.placement_revision
           OR current_placement.event_kind <> 'pre_pin_replaced'
           OR current_placement.state_kind <> 'unpinned'
           OR current_placement.placement_revision <> request.expected_placement_revision + 1
           OR current_placement.selector_kind <> 'identity'
           OR current_placement.selector_runner_id IS DISTINCT FROM NEW.new_runner_id
           OR current_placement.requested_sandbox_profile IS DISTINCT FROM NEW.sandbox_profile
           OR prior_placement.event_kind <> 'runner_lost_before_pin'
           OR prior_placement.state_kind <> 'runner_lost_before_pin'
           OR prior_placement.placement_revision IS DISTINCT FROM request.expected_placement_revision
           OR prior_placement.lost_runner_id IS DISTINCT FROM NEW.prior_runner_id
           OR NOT (
                (request.target_kind = 'runner'
                    AND request.target_runner_id IS NOT DISTINCT FROM NEW.new_runner_id)
                OR (request.target_kind = 'pending_enrollment'
                    AND target_enrollment IS NOT DISTINCT FROM NEW.target_enrollment_id
                    AND target_runner IS NOT DISTINCT FROM NEW.new_runner_id
                    AND target_predecessor_runner IS NOT DISTINCT FROM NEW.prior_runner_id
                    AND target_candidate_state = 'active'
                    AND target_predecessor_state = 'revoked')
           )
        THEN
            RAISE EXCEPTION 'applied pre-pin replacement lacks exact placement authority'
                USING ERRCODE = '23514';
        END IF;
        IF request.target_kind = 'runner' THEN
            SELECT enrollment_id INTO target_enrollment
              FROM runner_enrollment
             WHERE runner_id = NEW.new_runner_id
               AND state_kind = 'active';
        END IF;
        IF target_enrollment IS NOT NULL THEN
            SELECT current_registration.registration_revision
              INTO target_registration
              FROM runner_current_registration AS current_registration
             WHERE current_registration.enrollment_id = target_enrollment;
            SELECT authority.connection_epoch, authority.connection_event_ordinal,
                   connection.state_kind
              INTO target_connection_epoch, target_connection_event_ordinal,
                   target_connection_state
              FROM runner_connection_authority_head AS authority
              JOIN runner_connection_event AS connection
                ON connection.enrollment_id = authority.enrollment_id
               AND connection.connection_epoch = authority.connection_epoch
               AND connection.event_ordinal = authority.connection_event_ordinal
             WHERE authority.enrollment_id = target_enrollment;
        END IF;
        target_is_advertised := target_enrollment IS NOT NULL
            AND target_registration IS NOT NULL
            AND target_connection_state = 'connected'
            AND EXISTS (
                SELECT 1 FROM runner_registration_sandbox AS sandbox
                 WHERE sandbox.enrollment_id = target_enrollment
                   AND sandbox.registration_revision = target_registration
                   AND sandbox.sandbox_profile =
                        current_placement.requested_sandbox_profile
            )
            AND (
                current_placement.requested_credential_profile_name IS NULL
                OR EXISTS (
                    SELECT 1 FROM runner_registration_profile AS profile
                     WHERE profile.enrollment_id = target_enrollment
                       AND profile.registration_revision = target_registration
                       AND profile.credential_profile_name =
                            current_placement.requested_credential_profile_name
                )
            )
            AND (
                current_placement.workspace_requirement_kind = 'none'
                OR EXISTS (
                    SELECT 1 FROM runner_registration_workspace AS workspace
                     WHERE workspace.enrollment_id = target_enrollment
                       AND workspace.registration_revision = target_registration
                       AND workspace.workspace_kind = 'worktree_per_session'
                ) AND EXISTS (
                    SELECT 1 FROM runner_registration_repository AS repository
                     WHERE repository.enrollment_id = target_enrollment
                       AND repository.registration_revision = target_registration
                       AND repository.repository_key =
                            current_placement.requested_repository_key
                )
            )
            AND NOT EXISTS (
                SELECT 1
                  FROM runner_session_placement_permission_override AS permission_override
                 WHERE permission_override.session_id = current_placement.session_id
                   AND permission_override.event_ordinal = current_placement.event_ordinal
                   AND NOT EXISTS (
                        SELECT 1 FROM runner_registration_tool AS tool
                         WHERE tool.enrollment_id = target_enrollment
                           AND tool.registration_revision = target_registration
                           AND tool.tool_name = permission_override.tool_name
                   )
            );
        IF target_enrollment IS DISTINCT FROM NEW.target_enrollment_id
           OR target_registration IS DISTINCT FROM NEW.target_registration_revision
           OR target_connection_epoch IS DISTINCT FROM NEW.target_connection_epoch
           OR target_connection_event_ordinal IS DISTINCT FROM
                NEW.target_connection_event_ordinal
           OR NOT target_is_advertised
        THEN
            RAISE EXCEPTION 'applied pre-pin replacement lacks live advertised target authority'
                USING ERRCODE = '23514';
        END IF;
    ELSIF NEW.rejection_kind = 'session_not_found' THEN
        IF session_exists THEN
            RAISE EXCEPTION 'session-not-found replacement rejection names a session'
                USING ERRCODE = '23514';
        END IF;
    ELSIF NEW.rejection_kind = 'runner_placement_not_found' THEN
        IF NOT session_exists OR current_placement.event_ordinal IS NOT NULL THEN
            RAISE EXCEPTION 'placement-not-found replacement rejection is stale'
                USING ERRCODE = '23514';
        END IF;
    ELSE
        IF NOT session_exists
           OR current_placement.event_ordinal IS DISTINCT FROM NEW.placement_event_ordinal
           OR current_placement.placement_revision IS DISTINCT FROM NEW.placement_revision
           OR current_placement.state_kind IS DISTINCT FROM NEW.placement_state_kind
        THEN
            RAISE EXCEPTION 'runner replacement rejection lacks exact current placement'
                USING ERRCODE = '23514';
        END IF;
        IF NEW.rejection_kind = 'placement_revision_mismatch'
           AND current_placement.placement_revision = request.expected_placement_revision
        THEN
            RAISE EXCEPTION 'replacement revision mismatch records equal revisions'
                USING ERRCODE = '23514';
        ELSIF NEW.rejection_kind = 'placement_not_lost'
              AND current_placement.state_kind NOT IN ('unpinned', 'pinned', 'runner_abandoned')
        THEN
            RAISE EXCEPTION 'placement-not-lost rejection names a lost placement'
                USING ERRCODE = '23514';
        ELSIF NEW.rejection_kind IN (
            'replacement_same_runner', 'replacement_target_unavailable'
        ) THEN
            IF current_placement.state_kind NOT IN ('runner_lost_before_pin', 'runner_lost')
               OR current_placement.placement_revision <>
                    request.expected_placement_revision
               OR current_placement.lost_runner_id IS DISTINCT FROM NEW.prior_runner_id
            THEN
                RAISE EXCEPTION 'replacement target rejection lacks exact lost placement'
                    USING ERRCODE = '23514';
            END IF;
            IF NEW.rejection_kind = 'replacement_same_runner' THEN
                IF NEW.new_runner_id <> current_placement.lost_runner_id
                   OR NOT (
                        (request.target_kind = 'runner'
                            AND request.target_runner_id = NEW.new_runner_id)
                        OR (request.target_kind = 'pending_enrollment'
                            AND target_runner = NEW.new_runner_id)
                        OR (request.target_kind = 'same_runner_reenrollment'
                            AND request.target_runner_id = NEW.new_runner_id
                            AND current_placement.loss_source_kind <> 'registration')
                   )
                THEN
                    RAISE EXCEPTION 'same-runner rejection names a different runner'
                        USING ERRCODE = '23514';
                END IF;
            ELSIF request.target_kind IN ('runner', 'same_runner_reenrollment') THEN
                IF request.target_runner_id IS DISTINCT FROM NEW.new_runner_id
                   OR (request.target_kind = 'runner'
                       AND NEW.new_runner_id = current_placement.lost_runner_id)
                   OR (request.target_kind = 'same_runner_reenrollment'
                       AND (
                            NEW.new_runner_id <> current_placement.lost_runner_id
                            OR current_placement.loss_source_kind <> 'registration'
                       ))
                THEN
                    RAISE EXCEPTION 'replacement target rejection lacks its selected runner'
                        USING ERRCODE = '23514';
                END IF;
                SELECT enrollment_id INTO target_enrollment
                  FROM runner_enrollment
                 WHERE runner_id = NEW.new_runner_id
                   AND state_kind = 'active';
                IF target_enrollment IS NOT NULL THEN
                    SELECT current_registration.registration_revision
                      INTO target_registration
                      FROM runner_current_registration AS current_registration
                     WHERE current_registration.enrollment_id = target_enrollment;
                    SELECT connection.state_kind INTO target_connection_state
                      FROM runner_connection_authority_head AS authority
                      JOIN runner_connection_event AS connection
                        ON connection.enrollment_id = authority.enrollment_id
                       AND connection.connection_epoch = authority.connection_epoch
                       AND connection.event_ordinal = authority.connection_event_ordinal
                     WHERE authority.enrollment_id = target_enrollment;
                END IF;
                target_is_advertised := target_enrollment IS NOT NULL
                    AND target_registration IS NOT NULL
                    AND target_connection_state = 'connected'
                    AND EXISTS (
                        SELECT 1 FROM runner_registration_sandbox AS sandbox
                         WHERE sandbox.enrollment_id = target_enrollment
                           AND sandbox.registration_revision = target_registration
                           AND sandbox.sandbox_profile =
                                current_placement.requested_sandbox_profile
                    )
                    AND (
                        current_placement.requested_credential_profile_name IS NULL
                        OR EXISTS (
                            SELECT 1 FROM runner_registration_profile AS profile
                             WHERE profile.enrollment_id = target_enrollment
                               AND profile.registration_revision = target_registration
                               AND profile.credential_profile_name =
                                    current_placement.requested_credential_profile_name
                        )
                    )
                    AND (
                        current_placement.workspace_requirement_kind = 'none'
                        OR EXISTS (
                            SELECT 1 FROM runner_registration_workspace AS workspace
                             WHERE workspace.enrollment_id = target_enrollment
                               AND workspace.registration_revision = target_registration
                               AND workspace.workspace_kind = 'worktree_per_session'
                        ) AND EXISTS (
                            SELECT 1 FROM runner_registration_repository AS repository
                             WHERE repository.enrollment_id = target_enrollment
                               AND repository.registration_revision = target_registration
                               AND repository.repository_key =
                                    current_placement.requested_repository_key
                        )
                    )
                    AND NOT EXISTS (
                        SELECT 1
                          FROM runner_session_placement_permission_override AS permission_override
                         WHERE permission_override.session_id = current_placement.session_id
                           AND permission_override.event_ordinal = current_placement.event_ordinal
                           AND NOT EXISTS (
                                SELECT 1 FROM runner_registration_tool AS tool
                                 WHERE tool.enrollment_id = target_enrollment
                                   AND tool.registration_revision = target_registration
                                   AND tool.tool_name = permission_override.tool_name
                           )
                    );
                IF (NEW.target_unavailable_reason = 'not_current') <>
                        (target_enrollment IS NULL)
                   OR (NEW.target_unavailable_reason = 'not_connected') <>
                        (target_enrollment IS NOT NULL
                         AND target_connection_state IS DISTINCT FROM 'connected')
                   OR (NEW.target_unavailable_reason = 'not_advertised') <>
                        (target_enrollment IS NOT NULL
                         AND target_connection_state = 'connected'
                         AND NOT target_is_advertised)
                THEN
                    RAISE EXCEPTION 'replacement target rejection lacks exact runner authority'
                        USING ERRCODE = '23514';
                END IF;
            ELSIF request.target_kind = 'pending_enrollment' THEN
                IF NEW.target_unavailable_reason = 'pending_request_mismatch' THEN
                    IF target_candidate_state = 'pending'
                       AND target_predecessor_runner = current_placement.lost_runner_id
                    THEN
                        RAISE EXCEPTION 'pending mismatch contradicts current authority'
                            USING ERRCODE = '23514';
                    END IF;
                    IF (target_runner IS NULL AND NEW.new_runner_id IS NOT NULL)
                       OR (target_runner IS NOT NULL
                           AND target_runner IS DISTINCT FROM NEW.new_runner_id)
                    THEN
                        RAISE EXCEPTION 'pending mismatch names unrelated runner evidence'
                            USING ERRCODE = '23514';
                    END IF;
                ELSIF NEW.target_unavailable_reason = 'pending_request_disconnected' THEN
                    SELECT connection.state_kind INTO target_connection_state
                      FROM runner_connection_authority_head AS authority
                      JOIN runner_connection_event AS connection
                        ON connection.enrollment_id = authority.enrollment_id
                       AND connection.connection_epoch = authority.connection_epoch
                       AND connection.event_ordinal = authority.connection_event_ordinal
                     WHERE authority.enrollment_id = target_enrollment;
                    IF target_candidate_state <> 'pending'
                       OR target_predecessor_runner IS DISTINCT FROM
                            current_placement.lost_runner_id
                       OR target_runner IS DISTINCT FROM NEW.new_runner_id
                       OR target_connection_state = 'connected'
                    THEN
                        RAISE EXCEPTION 'pending disconnected rejection contradicts current authority'
                            USING ERRCODE = '23514';
                    END IF;
                ELSIF NEW.target_unavailable_reason = 'not_advertised' THEN
                    SELECT current_registration.registration_revision
                      INTO target_registration
                      FROM runner_current_registration AS current_registration
                     WHERE current_registration.enrollment_id = target_enrollment;
                    SELECT connection.state_kind INTO target_connection_state
                      FROM runner_connection_authority_head AS authority
                      JOIN runner_connection_event AS connection
                        ON connection.enrollment_id = authority.enrollment_id
                       AND connection.connection_epoch = authority.connection_epoch
                       AND connection.event_ordinal = authority.connection_event_ordinal
                     WHERE authority.enrollment_id = target_enrollment;
                    target_is_advertised := target_registration IS NOT NULL
                        AND EXISTS (
                            SELECT 1 FROM runner_registration_sandbox AS sandbox
                             WHERE sandbox.enrollment_id = target_enrollment
                               AND sandbox.registration_revision = target_registration
                               AND sandbox.sandbox_profile =
                                    current_placement.requested_sandbox_profile
                        )
                        AND (
                            current_placement.requested_credential_profile_name IS NULL
                            OR EXISTS (
                                SELECT 1 FROM runner_registration_profile AS profile
                                 WHERE profile.enrollment_id = target_enrollment
                                   AND profile.registration_revision = target_registration
                                   AND profile.credential_profile_name =
                                        current_placement.requested_credential_profile_name
                            )
                        )
                        AND (
                            current_placement.workspace_requirement_kind = 'none'
                            OR EXISTS (
                                SELECT 1 FROM runner_registration_workspace AS workspace
                                 WHERE workspace.enrollment_id = target_enrollment
                                   AND workspace.registration_revision = target_registration
                                   AND workspace.workspace_kind = 'worktree_per_session'
                            ) AND EXISTS (
                                SELECT 1 FROM runner_registration_repository AS repository
                                 WHERE repository.enrollment_id = target_enrollment
                                   AND repository.registration_revision = target_registration
                                   AND repository.repository_key =
                                        current_placement.requested_repository_key
                            )
                        )
                        AND NOT EXISTS (
                            SELECT 1
                              FROM runner_session_placement_permission_override AS permission_override
                             WHERE permission_override.session_id = current_placement.session_id
                               AND permission_override.event_ordinal = current_placement.event_ordinal
                               AND NOT EXISTS (
                                    SELECT 1 FROM runner_registration_tool AS tool
                                     WHERE tool.enrollment_id = target_enrollment
                                       AND tool.registration_revision = target_registration
                                       AND tool.tool_name = permission_override.tool_name
                               )
                        );
                    IF target_candidate_state <> 'pending'
                       OR target_predecessor_runner IS DISTINCT FROM
                            current_placement.lost_runner_id
                       OR target_runner IS DISTINCT FROM NEW.new_runner_id
                       OR target_connection_state <> 'connected'
                       OR target_is_advertised
                    THEN
                        RAISE EXCEPTION 'pending advertised rejection contradicts current authority'
                            USING ERRCODE = '23514';
                    END IF;
                ELSE
                    RAISE EXCEPTION 'pending target uses a direct-runner rejection reason'
                        USING ERRCODE = '23514';
                END IF;
            ELSE
                RAISE EXCEPTION 'runner replacement target kind is unavailable'
                    USING ERRCODE = '23514';
            END IF;
        END IF;
    END IF;
    RETURN NULL;
END;
$$;
