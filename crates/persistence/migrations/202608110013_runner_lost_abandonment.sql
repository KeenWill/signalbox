-- Terminalize one exact lost session placement under durable command authority.

-- Supersedes the durable-command constraints and completeness function from
-- 202608110012_runner_pending_successor_promotion.sql.
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
            'promote_pending_runner', 'abandon_lost_runner'
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
            'promote_pending_runner', 'abandon_lost_runner'
        ) AND storage_version = 1)
    );

CREATE TABLE abandon_lost_runner_command (
    command_id uuid PRIMARY KEY,
    command_kind text NOT NULL DEFAULT 'abandon_lost_runner',
    storage_version smallint NOT NULL DEFAULT 1,
    session_id uuid NOT NULL,
    expected_placement_revision numeric(20, 0) NOT NULL,
    result_kind text NOT NULL,
    rejection_kind text,
    placement_event_ordinal numeric(20, 0),
    placement_revision numeric(20, 0),
    placement_state_kind text,
    active_turn_id uuid,

    CONSTRAINT abandon_lost_runner_command_kind
        CHECK (command_kind = 'abandon_lost_runner'),
    CONSTRAINT abandon_lost_runner_command_version
        CHECK (storage_version = 1),
    CONSTRAINT abandon_lost_runner_command_positive_u64 CHECK (
        expected_placement_revision BETWEEN 1 AND 18446744073709551615
        AND (
            placement_event_ordinal IS NULL
            OR placement_event_ordinal BETWEEN 1 AND 18446744073709551615
        )
        AND (
            placement_revision IS NULL
            OR placement_revision BETWEEN 1 AND 18446744073709551615
        )
    ),
    CONSTRAINT abandon_lost_runner_command_result_closed
        CHECK (result_kind IN ('applied', 'rejected')),
    CONSTRAINT abandon_lost_runner_command_rejection_closed CHECK (
        rejection_kind IS NULL
        OR rejection_kind IN (
            'session_not_found', 'runner_placement_not_found',
            'placement_revision_mismatch', 'placement_not_lost',
            'active_turn_requires_existing_control'
        )
    ),
    CONSTRAINT abandon_lost_runner_command_state_closed CHECK (
        placement_state_kind IS NULL
        OR placement_state_kind IN (
            'unpinned', 'pinned', 'runner_lost_before_pin', 'runner_lost',
            'runner_abandoned'
        )
    ),
    CONSTRAINT abandon_lost_runner_command_result_shape CHECK (
        (
            result_kind = 'applied'
            AND rejection_kind IS NULL
            AND placement_event_ordinal IS NOT NULL
            AND placement_revision = expected_placement_revision
            AND placement_state_kind = 'runner_abandoned'
            AND active_turn_id IS NULL
        )
        OR (
            result_kind = 'rejected'
            AND rejection_kind IN (
                'session_not_found', 'runner_placement_not_found'
            )
            AND placement_event_ordinal IS NULL
            AND placement_revision IS NULL
            AND placement_state_kind IS NULL
            AND active_turn_id IS NULL
        )
        OR (
            result_kind = 'rejected'
            AND rejection_kind = 'placement_revision_mismatch'
            AND placement_event_ordinal IS NOT NULL
            AND placement_revision IS NOT NULL
            AND placement_revision <> expected_placement_revision
            AND placement_state_kind IS NOT NULL
            AND active_turn_id IS NULL
        )
        OR (
            result_kind = 'rejected'
            AND rejection_kind = 'placement_not_lost'
            AND placement_event_ordinal IS NOT NULL
            AND placement_revision = expected_placement_revision
            AND placement_state_kind IN (
                'unpinned', 'pinned', 'runner_abandoned'
            )
            AND active_turn_id IS NULL
        )
        OR (
            result_kind = 'rejected'
            AND rejection_kind = 'active_turn_requires_existing_control'
            AND placement_event_ordinal IS NOT NULL
            AND placement_revision = expected_placement_revision
            AND placement_state_kind IN (
                'runner_lost_before_pin', 'runner_lost'
            )
            AND active_turn_id IS NOT NULL
        )
    ),
    CONSTRAINT abandon_lost_runner_command_registry_fk
        FOREIGN KEY (command_id, command_kind, storage_version)
        REFERENCES durable_command (command_id, command_kind, storage_version)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT abandon_lost_runner_command_placement_fk
        FOREIGN KEY (
            session_id, placement_event_ordinal, placement_revision
        )
        REFERENCES runner_session_placement_record (
            session_id, event_ordinal, placement_revision
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT abandon_lost_runner_command_active_turn_fk
        FOREIGN KEY (active_turn_id, session_id)
        REFERENCES turn_lifecycle (turn_id, session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE FUNCTION require_abandon_lost_runner_authority()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    session_exists boolean;
    current_event_ordinal numeric(20, 0);
    current_state_kind text;
    active_turn_matches boolean;
BEGIN
    SELECT EXISTS (
        SELECT 1 FROM session WHERE session_id = NEW.session_id
    ) INTO session_exists;

    SELECT placement.event_ordinal, placement.state_kind
      INTO current_event_ordinal, current_state_kind
      FROM runner_current_session_placement AS current_placement
      JOIN runner_session_placement_record AS placement
        ON placement.session_id = current_placement.session_id
       AND placement.event_ordinal = current_placement.event_ordinal
     WHERE placement.session_id = NEW.session_id;

    IF NEW.result_kind = 'applied' THEN
        IF NOT session_exists
           OR current_event_ordinal IS DISTINCT FROM NEW.placement_event_ordinal
           OR current_state_kind <> 'runner_abandoned'
        THEN
            RAISE EXCEPTION
                'applied runner abandonment lacks exact terminal placement'
                USING ERRCODE = '23514';
        END IF;
    ELSIF NEW.rejection_kind = 'session_not_found' THEN
        IF session_exists THEN
            RAISE EXCEPTION
                'session-not-found abandonment rejection names a session'
                USING ERRCODE = '23514';
        END IF;
    ELSIF NEW.rejection_kind = 'runner_placement_not_found' THEN
        IF NOT session_exists OR current_event_ordinal IS NOT NULL THEN
            RAISE EXCEPTION
                'placement-not-found abandonment rejection is stale'
                USING ERRCODE = '23514';
        END IF;
    ELSE
        IF NOT session_exists
           OR current_event_ordinal IS DISTINCT FROM NEW.placement_event_ordinal
           OR current_state_kind IS DISTINCT FROM NEW.placement_state_kind
        THEN
            RAISE EXCEPTION
                'runner abandonment rejection lacks exact current placement'
                USING ERRCODE = '23514';
        END IF;
        IF NEW.rejection_kind = 'active_turn_requires_existing_control' THEN
            SELECT EXISTS (
                SELECT 1
                  FROM turn_lifecycle
                 WHERE turn_id = NEW.active_turn_id
                   AND session_id = NEW.session_id
                   AND state_kind = 'active'
                   AND NOT delegation_runtime_terminal
            ) INTO active_turn_matches;
            IF NOT active_turn_matches THEN
                RAISE EXCEPTION
                    'active-turn abandonment rejection lacks active authority'
                    USING ERRCODE = '23514';
            END IF;
        END IF;
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER abandon_lost_runner_is_authorized
AFTER INSERT ON abandon_lost_runner_command
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_abandon_lost_runner_authority();

CREATE TRIGGER abandon_lost_runner_command_is_append_only
BEFORE UPDATE OR DELETE ON abandon_lost_runner_command
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER abandon_lost_runner_command_rejects_truncate
BEFORE TRUNCATE ON abandon_lost_runner_command
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
        ELSE RAISE EXCEPTION 'unsupported durable command kind %', NEW.command_kind USING ERRCODE = '23514';
    END CASE;
    IF matching_records <> 1 THEN
        RAISE EXCEPTION 'durable command % requires exactly one % typed record', NEW.command_id, NEW.command_kind USING ERRCODE = '23503';
    END IF;
    RETURN NULL;
END;
$$;
