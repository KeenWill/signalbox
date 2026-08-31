-- Activate one provisioning-only pending enrollment in the same terminal
-- transaction that replaces its predecessor's lost pre-pin placement.

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
            AND placement_state_kind = 'runner_lost_before_pin'
            AND prior_runner_id IS NOT NULL
            AND new_runner_id = prior_runner_id
            AND sandbox_profile IS NULL)
        OR (result_kind = 'rejected'
            AND rejection_kind = 'replacement_target_unavailable'
            AND target_unavailable_reason IS NOT NULL
            AND placement_event_ordinal IS NOT NULL
            AND placement_revision IS NOT NULL
            AND placement_state_kind = 'runner_lost_before_pin'
            AND prior_runner_id IS NOT NULL
            AND (
                (target_unavailable_reason = 'pending_request_mismatch'
                    AND (new_runner_id IS NULL OR new_runner_id <> prior_runner_id))
                OR (target_unavailable_reason <> 'pending_request_mismatch'
                    AND new_runner_id IS NOT NULL
                    AND new_runner_id <> prior_runner_id)
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
            IF current_placement.state_kind <> 'runner_lost_before_pin'
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
                   )
                THEN
                    RAISE EXCEPTION 'same-runner rejection names a different runner'
                        USING ERRCODE = '23514';
                END IF;
            ELSIF request.target_kind = 'runner' THEN
                IF request.target_runner_id IS DISTINCT FROM NEW.new_runner_id THEN
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
                RAISE EXCEPTION 'pre-pin replacement target kind is unavailable'
                    USING ERRCODE = '23514';
            END IF;
        END IF;
    END IF;
    RETURN NULL;
END;
$$;

-- Supersedes the pending-enrollment completeness function from migration
-- 202608110012. Exactly one deployment promotion or one pending-target session
-- replacement may authenticate the pending-to-active transition.
CREATE OR REPLACE FUNCTION assert_runner_pending_enrollment_complete(
    checked_enrollment uuid
)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    candidate_state text;
    relation_count bigint;
    valid_relation_count bigint;
    applied_activation_count bigint;
BEGIN
    SELECT state_kind
      INTO candidate_state
      FROM runner_enrollment
     WHERE enrollment_id = checked_enrollment;
    IF NOT FOUND THEN
        RETURN;
    END IF;

    SELECT count(*),
           count(*) FILTER (
               WHERE receipt.authority_kind = 'replacement_pending'
                 AND loss.loss_epoch = pending.predecessor_loss_epoch
                 AND pending_audit.state_kind = 'pending'
                 AND (
                    (candidate_state = 'pending'
                        AND predecessor.state_kind = 'active'
                        AND active_audit.enrollment_id IS NULL)
                    OR (candidate_state IN ('active', 'revoked')
                        AND predecessor.state_kind = 'revoked'
                        AND active_audit.state_kind = 'active')
                 )
           ),
           count(DISTINCT promotion.command_id) FILTER (
               WHERE promotion.result_kind = 'applied'
                 AND promotion.pending_request_id = pending.request_id
                 AND promotion.result_enrollment_id = pending.enrollment_id
                 AND promotion.result_registration_revision =
                        receipt.registration_revision
                 AND pending_audit.state_kind = 'pending'
                 AND active_audit.state_kind = 'active'
                 AND predecessor.state_kind = 'revoked'
                 AND promotion_candidate_connection.state_kind = 'connected'
                 AND promotion_predecessor_connection.state_kind = 'lost'
                 AND promotion.predecessor_enrollment_id =
                        pending.predecessor_enrollment_id
           )
           + count(DISTINCT replacement_result.command_id) FILTER (
               WHERE replacement_command.target_kind = 'pending_enrollment'
                 AND replacement_command.target_pending_request_id = pending.request_id
                 AND replacement_result.result_kind = 'applied'
                 AND replacement_result.target_enrollment_id = pending.enrollment_id
                 AND replacement_result.target_registration_revision =
                        receipt.registration_revision
                 AND replacement_result.new_runner_id = candidate.runner_id
                 AND replacement_result.prior_runner_id = predecessor.runner_id
                 AND replacement_candidate_connection.state_kind = 'connected'
                 AND pending_audit.state_kind = 'pending'
                 AND active_audit.state_kind = 'active'
                 AND predecessor.state_kind = 'revoked'
           )
      INTO relation_count, valid_relation_count, applied_activation_count
      FROM runner_pending_enrollment AS pending
      JOIN runner_enrollment_request_receipt AS receipt
        ON receipt.request_id = pending.request_id
       AND receipt.enrollment_id = pending.enrollment_id
      JOIN runner_connection_loss_epoch AS loss
        ON loss.enrollment_id = pending.predecessor_enrollment_id
       AND loss.loss_epoch = pending.predecessor_loss_epoch
      JOIN runner_enrollment_audit AS pending_audit
        ON pending_audit.enrollment_id = pending.enrollment_id
       AND pending_audit.revision = 1
      LEFT JOIN runner_enrollment_audit AS active_audit
        ON active_audit.enrollment_id = pending.enrollment_id
       AND active_audit.revision = 2
      JOIN runner_enrollment AS candidate
        ON candidate.enrollment_id = pending.enrollment_id
      JOIN runner_enrollment AS predecessor
        ON predecessor.enrollment_id = pending.predecessor_enrollment_id
      LEFT JOIN promote_pending_runner_command AS promotion
        ON promotion.pending_request_id = pending.request_id
       AND promotion.result_enrollment_id = pending.enrollment_id
       AND promotion.result_kind = 'applied'
      LEFT JOIN runner_connection_event AS promotion_candidate_connection
        ON promotion_candidate_connection.enrollment_id =
            promotion.result_enrollment_id
       AND promotion_candidate_connection.connection_epoch =
            promotion.result_connection_epoch
       AND promotion_candidate_connection.event_ordinal =
            promotion.result_connection_event_ordinal
      LEFT JOIN runner_connection_loss_epoch AS promotion_predecessor_loss
        ON promotion_predecessor_loss.enrollment_id =
            promotion.predecessor_enrollment_id
       AND promotion_predecessor_loss.loss_epoch =
            promotion.predecessor_loss_epoch
      LEFT JOIN runner_connection_event AS promotion_predecessor_connection
        ON promotion_predecessor_connection.enrollment_id =
            promotion_predecessor_loss.enrollment_id
       AND promotion_predecessor_connection.connection_epoch =
            promotion_predecessor_loss.connection_epoch
       AND promotion_predecessor_connection.event_ordinal =
            promotion_predecessor_loss.connection_event_ordinal
      LEFT JOIN replace_lost_runner_command AS replacement_command
        ON replacement_command.target_kind = 'pending_enrollment'
       AND replacement_command.target_pending_request_id = pending.request_id
       AND EXISTS (
            SELECT 1
              FROM replace_lost_runner_result AS applied_replacement
             WHERE applied_replacement.command_id = replacement_command.command_id
               AND applied_replacement.result_kind = 'applied'
               AND applied_replacement.target_enrollment_id = pending.enrollment_id
       )
      LEFT JOIN replace_lost_runner_result AS replacement_result
        ON replacement_result.command_id = replacement_command.command_id
       AND replacement_result.result_kind = 'applied'
       AND replacement_result.target_enrollment_id = pending.enrollment_id
      LEFT JOIN runner_connection_event AS replacement_candidate_connection
        ON replacement_candidate_connection.enrollment_id =
            replacement_result.target_enrollment_id
       AND replacement_candidate_connection.connection_epoch =
            replacement_result.target_connection_epoch
       AND replacement_candidate_connection.event_ordinal =
            replacement_result.target_connection_event_ordinal
     WHERE pending.enrollment_id = checked_enrollment;

    IF relation_count = 0 AND candidate_state <> 'pending' THEN
        RETURN;
    END IF;
    IF ROW(relation_count, valid_relation_count) IS DISTINCT FROM ROW(1::bigint, 1::bigint)
       OR (candidate_state = 'pending' AND applied_activation_count <> 0)
       OR (candidate_state IN ('active', 'revoked') AND applied_activation_count <> 1)
    THEN
        RAISE EXCEPTION
            'pending runner enrollment lacks exact activation authority'
            USING ERRCODE = '23514';
    END IF;
END;
$$;

CREATE FUNCTION require_replace_lost_runner_pending_activation_complete()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.result_kind = 'applied' THEN
        PERFORM assert_runner_pending_enrollment_complete(
            NEW.target_enrollment_id
        );
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER replace_lost_runner_pending_activation_is_complete
AFTER INSERT ON replace_lost_runner_result
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_replace_lost_runner_pending_activation_complete();
