-- Replace one exact runner lost before its first pin under durable command authority.

-- Supersedes the durable-command constraints and completeness function from
-- 202608110013_runner_lost_abandonment.sql.
ALTER TABLE durable_command
    DROP CONSTRAINT durable_command_kind_closed,
    DROP CONSTRAINT durable_command_storage_version_supported,
    ADD CONSTRAINT durable_command_kind_closed CHECK (
        command_kind IN (
            'create_session', 'create_session_from_imported_frontier',
            'replace_session_defaults', 'replace_session_metadata',
            'submit_input', 'decide_tool_request', 'review_workflow',
            'review_orchestration', 'compact_session', 'goal',
            'update_session_placement', 'register_workspace',
            'mint_git_remote', 'withdraw_git_remote',
            'promote_pending_runner', 'abandon_lost_runner',
            'replace_lost_runner'
        )
    ),
    ADD CONSTRAINT durable_command_storage_version_supported CHECK (
        (command_kind = 'create_session'
            AND storage_version IN (1, 2, 3, 4, 5, 6, 7, 8))
        OR (command_kind = 'replace_session_defaults'
            AND storage_version IN (1, 2, 3, 4))
        OR (command_kind = 'create_session_from_imported_frontier'
            AND storage_version IN (1, 2, 3, 5, 6))
        OR (command_kind = 'submit_input' AND storage_version IN (1, 2))
        OR (command_kind IN (
            'replace_session_metadata', 'decide_tool_request',
            'review_workflow', 'review_orchestration', 'compact_session',
            'goal', 'update_session_placement', 'register_workspace',
            'mint_git_remote', 'withdraw_git_remote',
            'promote_pending_runner', 'abandon_lost_runner',
            'replace_lost_runner'
        ) AND storage_version = 1)
    );

CREATE TABLE replace_lost_runner_command (
    command_id uuid PRIMARY KEY,
    command_kind text NOT NULL DEFAULT 'replace_lost_runner',
    storage_version smallint NOT NULL DEFAULT 1,
    session_id uuid NOT NULL,
    expected_placement_revision numeric(20, 0) NOT NULL,
    target_kind text NOT NULL,
    target_runner_id uuid,
    target_pending_request_id uuid,

    CONSTRAINT replace_lost_runner_command_identity UNIQUE (command_id, session_id),

    CONSTRAINT replace_lost_runner_command_kind
        CHECK (command_kind = 'replace_lost_runner'),
    CONSTRAINT replace_lost_runner_command_version CHECK (storage_version = 1),
    CONSTRAINT replace_lost_runner_command_positive_revision CHECK (
        expected_placement_revision BETWEEN 1 AND 18446744073709551615
    ),
    CONSTRAINT replace_lost_runner_command_target_closed CHECK (
        target_kind IN ('runner', 'pending_enrollment', 'same_runner_reenrollment')
    ),
    CONSTRAINT replace_lost_runner_command_target_shape CHECK (
        (target_kind IN ('runner', 'same_runner_reenrollment')
            AND target_runner_id IS NOT NULL
            AND target_pending_request_id IS NULL)
        OR (target_kind = 'pending_enrollment'
            AND target_runner_id IS NULL
            AND target_pending_request_id IS NOT NULL)
    ),
    CONSTRAINT replace_lost_runner_command_registry_fk
        FOREIGN KEY (command_id, command_kind, storage_version)
        REFERENCES durable_command (command_id, command_kind, storage_version)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE TABLE replace_lost_runner_result (
    command_id uuid PRIMARY KEY,
    session_id uuid NOT NULL,
    result_kind text NOT NULL,
    rejection_kind text,
    target_unavailable_reason text,
    placement_event_ordinal numeric(20, 0),
    placement_revision numeric(20, 0),
    placement_state_kind text,
    prior_runner_id uuid,
    new_runner_id uuid,
    sandbox_profile text,
    target_enrollment_id uuid,
    target_registration_revision numeric(20, 0),
    target_connection_epoch numeric(20, 0),
    target_connection_event_ordinal numeric(20, 0),

    CONSTRAINT replace_lost_runner_result_kind_closed
        CHECK (result_kind IN ('applied', 'rejected')),
    CONSTRAINT replace_lost_runner_result_rejection_closed CHECK (
        rejection_kind IS NULL
        OR rejection_kind IN (
            'session_not_found', 'runner_placement_not_found',
            'placement_revision_mismatch', 'placement_not_lost',
            'replacement_same_runner', 'replacement_target_unavailable'
        )
    ),
    CONSTRAINT replace_lost_runner_result_target_reason_closed CHECK (
        target_unavailable_reason IS NULL
        OR target_unavailable_reason IN (
            'not_connected', 'not_current', 'not_advertised',
            'pending_request_mismatch', 'pending_request_disconnected'
        )
    ),
    CONSTRAINT replace_lost_runner_result_state_closed CHECK (
        placement_state_kind IS NULL
        OR placement_state_kind IN (
            'unpinned', 'pinned', 'runner_lost_before_pin', 'runner_lost',
            'runner_abandoned'
        )
    ),
    CONSTRAINT replace_lost_runner_result_sandbox_closed CHECK (
        sandbox_profile IS NULL
        OR sandbox_profile IN ('workspace_restricted', 'ambient')
    ),
    CONSTRAINT replace_lost_runner_result_positive_u64 CHECK (
        (placement_event_ordinal IS NULL
            OR placement_event_ordinal BETWEEN 1 AND 18446744073709551615)
        AND (placement_revision IS NULL
            OR placement_revision BETWEEN 1 AND 18446744073709551615)
        AND (target_registration_revision IS NULL
            OR target_registration_revision BETWEEN 1 AND 18446744073709551615)
        AND (target_connection_epoch IS NULL
            OR target_connection_epoch BETWEEN 1 AND 18446744073709551615)
        AND (target_connection_event_ordinal IS NULL
            OR target_connection_event_ordinal BETWEEN 1 AND 18446744073709551615)
    ),
    CONSTRAINT replace_lost_runner_result_target_authority_shape CHECK (
        (result_kind = 'applied'
            AND target_enrollment_id IS NOT NULL
            AND target_registration_revision IS NOT NULL
            AND target_connection_epoch IS NOT NULL
            AND target_connection_event_ordinal IS NOT NULL)
        OR (result_kind = 'rejected'
            AND target_enrollment_id IS NULL
            AND target_registration_revision IS NULL
            AND target_connection_epoch IS NULL
            AND target_connection_event_ordinal IS NULL)
    ),
    CONSTRAINT replace_lost_runner_result_shape CHECK (
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
            AND new_runner_id IS NOT NULL
            AND prior_runner_id <> new_runner_id
            AND sandbox_profile IS NULL)
    ),
    CONSTRAINT replace_lost_runner_result_command_fk
        FOREIGN KEY (command_id, session_id)
        REFERENCES replace_lost_runner_command (command_id, session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT replace_lost_runner_result_placement_fk
        FOREIGN KEY (
            session_id, placement_event_ordinal, placement_revision
        )
        REFERENCES runner_session_placement_record (
            session_id, event_ordinal, placement_revision
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT replace_lost_runner_result_target_registration_fk
        FOREIGN KEY (target_enrollment_id, target_registration_revision)
        REFERENCES runner_registration (enrollment_id, registration_revision)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT replace_lost_runner_result_target_connection_fk
        FOREIGN KEY (
            target_enrollment_id, target_connection_epoch,
            target_connection_event_ordinal
        )
        REFERENCES runner_connection_event (
            enrollment_id, connection_epoch, event_ordinal
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE FUNCTION require_replace_lost_runner_result_authority()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    request replace_lost_runner_command%ROWTYPE;
    current_placement runner_session_placement_record%ROWTYPE;
    prior_placement runner_session_placement_record%ROWTYPE;
    session_exists boolean;
    target_enrollment uuid;
    target_registration numeric(20, 0);
    target_connection_epoch numeric(20, 0);
    target_connection_event_ordinal numeric(20, 0);
    target_connection_state text;
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
           OR request.target_kind <> 'runner'
           OR request.target_runner_id IS DISTINCT FROM NEW.new_runner_id
           OR prior_placement.event_kind <> 'runner_lost_before_pin'
           OR prior_placement.state_kind <> 'runner_lost_before_pin'
           OR prior_placement.placement_revision IS DISTINCT FROM request.expected_placement_revision
           OR prior_placement.lost_runner_id IS DISTINCT FROM NEW.prior_runner_id
        THEN
            RAISE EXCEPTION 'applied pre-pin replacement lacks exact placement authority'
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
               OR request.target_kind <> 'runner'
               OR request.target_runner_id IS DISTINCT FROM NEW.new_runner_id
            THEN
                RAISE EXCEPTION 'replacement target rejection lacks exact lost placement'
                    USING ERRCODE = '23514';
            END IF;
            IF NEW.rejection_kind = 'replacement_same_runner' THEN
                IF NEW.new_runner_id <> current_placement.lost_runner_id THEN
                    RAISE EXCEPTION 'same-runner rejection names a different runner'
                        USING ERRCODE = '23514';
                END IF;
            ELSE
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
            END IF;
        END IF;
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER replace_lost_runner_result_is_authorized
AFTER INSERT ON replace_lost_runner_result
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_replace_lost_runner_result_authority();

CREATE TRIGGER replace_lost_runner_command_is_append_only
BEFORE UPDATE OR DELETE ON replace_lost_runner_command
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER replace_lost_runner_command_rejects_truncate
BEFORE TRUNCATE ON replace_lost_runner_command
FOR EACH STATEMENT
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER replace_lost_runner_result_is_append_only
BEFORE UPDATE OR DELETE ON replace_lost_runner_result
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER replace_lost_runner_result_rejects_truncate
BEFORE TRUNCATE ON replace_lost_runner_result
FOR EACH STATEMENT
EXECUTE FUNCTION reject_immutable_record_change();

CREATE OR REPLACE FUNCTION require_durable_command_typed_record()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE matching_records bigint;
BEGIN
    IF NEW.command_kind <> 'review_orchestration' AND EXISTS (
        SELECT 1 FROM review_orchestration_command_recovery
         WHERE command_id = NEW.command_id
    ) THEN
        RAISE EXCEPTION 'durable command % is reserved by review orchestration recovery', NEW.command_id
            USING ERRCODE = '23505';
    END IF;
    CASE NEW.command_kind
        WHEN 'create_session' THEN SELECT count(*) INTO matching_records FROM create_session_command WHERE command_id = NEW.command_id;
        WHEN 'create_session_from_imported_frontier' THEN SELECT count(*) INTO matching_records FROM create_session_from_imported_frontier_command WHERE command_id = NEW.command_id;
        WHEN 'replace_session_defaults' THEN SELECT count(*) INTO matching_records FROM replace_session_defaults_command WHERE command_id = NEW.command_id;
        WHEN 'replace_session_metadata' THEN SELECT count(*) INTO matching_records FROM replace_session_metadata_command WHERE command_id = NEW.command_id;
        WHEN 'submit_input' THEN SELECT count(*) INTO matching_records FROM submit_input_command WHERE command_id = NEW.command_id;
        WHEN 'decide_tool_request' THEN SELECT count(*) INTO matching_records FROM decide_tool_request_command WHERE command_id = NEW.command_id;
        WHEN 'review_workflow' THEN SELECT count(*) INTO matching_records FROM review_workflow_command WHERE command_id = NEW.command_id;
        WHEN 'review_orchestration' THEN SELECT (SELECT count(*) FROM review_orchestration_command WHERE command_id = NEW.command_id) + (SELECT count(*) FROM review_orchestration_command_intent WHERE command_id = NEW.command_id) INTO matching_records;
        WHEN 'compact_session' THEN SELECT count(*) INTO matching_records FROM compact_session_command WHERE command_id = NEW.command_id;
        WHEN 'goal' THEN SELECT count(*) INTO matching_records FROM goal_command WHERE command_id = NEW.command_id;
        WHEN 'update_session_placement' THEN SELECT count(*) INTO matching_records FROM update_session_placement_command WHERE command_id = NEW.command_id;
        WHEN 'register_workspace' THEN SELECT count(*) INTO matching_records FROM workspace WHERE command_id = NEW.command_id;
        WHEN 'mint_git_remote' THEN SELECT count(*) INTO matching_records FROM configured_git_remote_mint WHERE command_id = NEW.command_id;
        WHEN 'withdraw_git_remote' THEN SELECT count(*) INTO matching_records FROM configured_git_remote_withdrawal WHERE command_id = NEW.command_id;
        WHEN 'promote_pending_runner' THEN SELECT count(*) INTO matching_records FROM promote_pending_runner_command WHERE command_id = NEW.command_id;
        WHEN 'abandon_lost_runner' THEN SELECT count(*) INTO matching_records FROM abandon_lost_runner_command WHERE command_id = NEW.command_id;
        WHEN 'replace_lost_runner' THEN SELECT count(*) INTO matching_records FROM replace_lost_runner_command WHERE command_id = NEW.command_id;
        ELSE RAISE EXCEPTION 'unsupported durable command kind %', NEW.command_kind USING ERRCODE = '23514';
    END CASE;
    IF matching_records <> 1 THEN
        RAISE EXCEPTION 'durable command % requires exactly one % typed record', NEW.command_id, NEW.command_kind USING ERRCODE = '23503';
    END IF;
    RETURN NULL;
END;
$$;
