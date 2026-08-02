-- Durable delegated-session relationships and their append-only histories.

ALTER TABLE session
    ADD COLUMN spawning_tool_request_id uuid,
    DROP CONSTRAINT session_creation_cause_closed;

ALTER TABLE session
    ADD CONSTRAINT session_creation_cause_closed
        CHECK (creation_cause IN ('owner_initiated', 'delegated')),
    ADD CONSTRAINT session_delegated_cause_shape CHECK (
        (creation_cause = 'owner_initiated' AND spawning_tool_request_id IS NULL)
        OR (creation_cause = 'delegated' AND ancestry_kind = 'none'
            AND spawning_tool_request_id IS NOT NULL)
    ),
    ADD CONSTRAINT session_spawning_request_fk
        FOREIGN KEY (spawning_tool_request_id)
        REFERENCES tool_request(request_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    ADD CONSTRAINT session_delegated_provenance_key
        UNIQUE (spawning_tool_request_id, session_id);

CREATE TABLE session_delegation (
    spawning_tool_request_id uuid PRIMARY KEY,
    parent_session_id uuid NOT NULL,
    parent_turn_id uuid NOT NULL,
    child_session_id uuid NOT NULL UNIQUE,
    UNIQUE (spawning_tool_request_id, child_session_id),
    policy_kind text NOT NULL CHECK (policy_kind IN ('background', 'bound')),
    on_parent_stopped text CHECK (
        on_parent_stopped IS NULL OR on_parent_stopped IN ('keep_running', 'stop', 'cancel')
    ),
    on_parent_cancelled text CHECK (
        on_parent_cancelled IS NULL OR on_parent_cancelled IN ('keep_running', 'stop', 'cancel')
    ),
    CONSTRAINT session_delegation_distinct_sessions
        CHECK (parent_session_id <> child_session_id),
    CONSTRAINT session_delegation_policy_shape CHECK (
        (policy_kind = 'background'
            AND on_parent_stopped IS NULL AND on_parent_cancelled IS NULL)
        OR (policy_kind = 'bound'
            AND on_parent_stopped IS NOT NULL AND on_parent_cancelled IS NOT NULL)
    ),
    CONSTRAINT session_delegation_parent_request_fk
        FOREIGN KEY (spawning_tool_request_id, parent_turn_id, parent_session_id)
        REFERENCES tool_request(request_id, turn_id, session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT session_delegation_child_fk
        FOREIGN KEY (spawning_tool_request_id, child_session_id)
        REFERENCES session(spawning_tool_request_id, session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED
);

ALTER TABLE session
    ADD CONSTRAINT session_delegation_relation_fk
        FOREIGN KEY (spawning_tool_request_id, session_id)
        REFERENCES session_delegation(spawning_tool_request_id, child_session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;

CREATE INDEX session_delegation_by_parent
    ON session_delegation(parent_session_id, spawning_tool_request_id);

CREATE TABLE session_delegation_wait (
    awaiting_tool_request_id uuid PRIMARY KEY,
    spawning_tool_request_id uuid NOT NULL,
    parent_session_id uuid NOT NULL,
    parent_turn_id uuid NOT NULL,
    child_session_id uuid NOT NULL,
    wait_mode text NOT NULL CHECK (wait_mode IN ('foreground', 'background')),
    CHECK (awaiting_tool_request_id <> spawning_tool_request_id),
    FOREIGN KEY (awaiting_tool_request_id, parent_turn_id, parent_session_id)
        REFERENCES tool_request(request_id, turn_id, session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (spawning_tool_request_id, child_session_id)
        REFERENCES session_delegation(spawning_tool_request_id, child_session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT
);

CREATE INDEX session_delegation_wait_by_relation
    ON session_delegation_wait(spawning_tool_request_id, awaiting_tool_request_id);

CREATE TABLE session_delegation_event (
    spawning_tool_request_id uuid NOT NULL,
    event_ordinal numeric(20, 0) NOT NULL CHECK (
        event_ordinal BETWEEN 1 AND 18446744073709551615
    ),
    event_kind text NOT NULL CHECK (
        event_kind IN ('spawned', 'message_delivered', 'outcome_recorded')
    ),
    outcome_kind text CHECK (
        outcome_kind IS NULL OR outcome_kind IN (
            'result_returned', 'child_failed', 'child_stopped',
            'child_cancelled', 'continue_running'
        )
    ),
    reason_kind text CHECK (
        reason_kind IS NULL OR reason_kind IN (
            'child_completed', 'child_execution_failed',
            'parent_stopped_parent_alone', 'parent_stopped_parent_and_descendants',
            'parent_cancelled_parent_alone', 'parent_cancelled_parent_and_descendants'
        )
    ),
    provenance_kind text NOT NULL CHECK (
        provenance_kind IN ('tool_request', 'child_turn', 'parent_command')
    ),
    provenance_session_id uuid NOT NULL,
    provenance_turn_id uuid,
    provenance_tool_request_id uuid,
    provenance_command_id uuid,
    PRIMARY KEY (spawning_tool_request_id, event_ordinal),
    UNIQUE (spawning_tool_request_id, event_ordinal, event_kind),
    UNIQUE (spawning_tool_request_id, event_ordinal, event_kind, outcome_kind),
    CONSTRAINT session_delegation_event_shape CHECK (
        (event_kind IN ('spawned', 'message_delivered')
            AND outcome_kind IS NULL AND reason_kind IS NULL)
        OR (event_kind = 'outcome_recorded'
            AND outcome_kind IS NOT NULL AND reason_kind IS NOT NULL)
    ),
    CONSTRAINT session_delegation_event_provenance_shape CHECK (
        (provenance_kind = 'tool_request' AND provenance_turn_id IS NOT NULL
            AND provenance_tool_request_id IS NOT NULL AND provenance_command_id IS NULL)
        OR (provenance_kind = 'child_turn' AND provenance_turn_id IS NOT NULL
            AND provenance_tool_request_id IS NULL AND provenance_command_id IS NULL)
        OR (provenance_kind = 'parent_command' AND provenance_turn_id IS NULL
            AND provenance_tool_request_id IS NULL AND provenance_command_id IS NOT NULL)
    ),
    CONSTRAINT session_delegation_spawn_provenance CHECK (
        event_kind <> 'spawned'
        OR (provenance_kind = 'tool_request'
            AND provenance_tool_request_id = spawning_tool_request_id)
    ),
    FOREIGN KEY (spawning_tool_request_id)
        REFERENCES session_delegation(spawning_tool_request_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT,
    FOREIGN KEY (provenance_tool_request_id, provenance_turn_id, provenance_session_id)
        REFERENCES tool_request(request_id, turn_id, session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (provenance_turn_id, provenance_session_id)
        REFERENCES turn_lifecycle(turn_id, session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (provenance_command_id)
        REFERENCES durable_command(command_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED
);

CREATE TABLE session_message (
    message_id uuid PRIMARY KEY,
    spawning_tool_request_id uuid NOT NULL,
    event_ordinal numeric(20, 0) NOT NULL,
    event_kind text NOT NULL CHECK (event_kind = 'message_delivered'),
    direction text NOT NULL CHECK (direction IN ('parent_to_child', 'child_to_parent')),
    content_text text NOT NULL CHECK (
        octet_length(content_text) BETWEEN 1 AND 1048576
    ),
    UNIQUE (spawning_tool_request_id, event_ordinal),
    FOREIGN KEY (spawning_tool_request_id, event_ordinal, event_kind)
        REFERENCES session_delegation_event(
            spawning_tool_request_id, event_ordinal, event_kind
        )
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED
);

CREATE TABLE session_child_result (
    spawning_tool_request_id uuid PRIMARY KEY,
    event_ordinal numeric(20, 0) NOT NULL,
    event_kind text NOT NULL CHECK (event_kind = 'outcome_recorded'),
    outcome_kind text NOT NULL CHECK (
        outcome_kind IN ('result_returned', 'child_failed', 'child_stopped', 'child_cancelled')
    ),
    content_text text CHECK (
        content_text IS NULL OR octet_length(content_text) BETWEEN 1 AND 1048576
    ),
    CONSTRAINT session_child_result_shape CHECK (
        (outcome_kind = 'result_returned' AND content_text IS NOT NULL)
        OR (outcome_kind <> 'result_returned' AND content_text IS NULL)
    ),
    FOREIGN KEY (spawning_tool_request_id, event_ordinal, event_kind, outcome_kind)
        REFERENCES session_delegation_event(
            spawning_tool_request_id, event_ordinal, event_kind, outcome_kind
        )
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED
);

CREATE FUNCTION guard_session_delegation_event_append()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE latest numeric(20, 0);
BEGIN
    PERFORM 1 FROM session_delegation
     WHERE spawning_tool_request_id = NEW.spawning_tool_request_id FOR UPDATE;
    SELECT max(event_ordinal) INTO latest FROM session_delegation_event
     WHERE spawning_tool_request_id = NEW.spawning_tool_request_id;
    IF (latest IS NULL AND NEW.event_ordinal <> 1)
        OR (latest IS NOT NULL AND NEW.event_ordinal <> latest + 1) THEN
        RAISE EXCEPTION 'delegation events must append contiguously'
            USING ERRCODE = '23514', CONSTRAINT = 'session_delegation_event_contiguous';
    END IF;
    IF latest IS NOT NULL AND EXISTS (
        SELECT 1 FROM session_child_result
         WHERE spawning_tool_request_id = NEW.spawning_tool_request_id
    ) THEN
        RAISE EXCEPTION 'terminal delegation history is immutable'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER session_delegation_event_append_guard
BEFORE INSERT ON session_delegation_event
FOR EACH ROW EXECUTE FUNCTION guard_session_delegation_event_append();

CREATE FUNCTION require_session_delegation_event_payload()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    payload_count bigint;
    stored_direction text;
    relation_parent uuid;
    relation_child uuid;
    relation_policy text;
    stopped_action text;
    cancelled_action text;
    expected_outcome text;
BEGIN
    SELECT parent_session_id, child_session_id, policy_kind,
           on_parent_stopped, on_parent_cancelled
      INTO relation_parent, relation_child, relation_policy,
           stopped_action, cancelled_action
      FROM session_delegation
     WHERE spawning_tool_request_id = NEW.spawning_tool_request_id;
    SELECT CASE NEW.event_kind
        WHEN 'message_delivered' THEN (SELECT count(*) FROM session_message
            WHERE spawning_tool_request_id = NEW.spawning_tool_request_id
              AND event_ordinal = NEW.event_ordinal)
        WHEN 'outcome_recorded' THEN CASE WHEN NEW.outcome_kind = 'continue_running' THEN 0
            ELSE (SELECT count(*) FROM session_child_result
                WHERE spawning_tool_request_id = NEW.spawning_tool_request_id
                  AND event_ordinal = NEW.event_ordinal
                  AND outcome_kind = NEW.outcome_kind) END
        ELSE 0 END INTO payload_count;
    IF (NEW.event_kind = 'message_delivered' AND payload_count <> 1)
        OR (NEW.event_kind = 'outcome_recorded'
            AND NEW.outcome_kind <> 'continue_running' AND payload_count <> 1) THEN
        RAISE EXCEPTION 'delegation event requires its exact payload row'
            USING ERRCODE = '23503', CONSTRAINT = 'session_delegation_event_requires_payload';
    END IF;
    IF NEW.event_kind = 'spawned' AND NOT (
        NEW.provenance_kind = 'tool_request'
        AND NEW.provenance_session_id = relation_parent
        AND NEW.provenance_tool_request_id = NEW.spawning_tool_request_id
    ) THEN
        RAISE EXCEPTION 'spawn provenance does not match delegation parent'
            USING ERRCODE = '23514', CONSTRAINT = 'session_delegation_event_semantics';
    ELSIF NEW.event_kind = 'message_delivered' THEN
        SELECT direction INTO stored_direction FROM session_message
         WHERE spawning_tool_request_id = NEW.spawning_tool_request_id
           AND event_ordinal = NEW.event_ordinal;
        IF NEW.provenance_kind <> 'tool_request'
            OR (stored_direction = 'parent_to_child'
                AND NEW.provenance_session_id <> relation_parent)
            OR (stored_direction = 'child_to_parent'
                AND NEW.provenance_session_id <> relation_child) THEN
            RAISE EXCEPTION 'message direction does not match delegation provenance'
                USING ERRCODE = '23514', CONSTRAINT = 'session_delegation_event_semantics';
        END IF;
    ELSIF NEW.event_kind = 'outcome_recorded' THEN
        IF NEW.reason_kind = 'child_completed' THEN
            IF NEW.outcome_kind <> 'result_returned'
                OR NEW.provenance_kind <> 'tool_request'
                OR NEW.provenance_session_id <> relation_child THEN
                RAISE EXCEPTION 'child completion has invalid provenance or outcome'
                    USING ERRCODE = '23514', CONSTRAINT = 'session_delegation_event_semantics';
            END IF;
        ELSIF NEW.reason_kind = 'child_execution_failed' THEN
            IF NEW.outcome_kind <> 'child_failed'
                OR NEW.provenance_kind <> 'child_turn'
                OR NEW.provenance_session_id <> relation_child THEN
                RAISE EXCEPTION 'child failure has invalid provenance or outcome'
                    USING ERRCODE = '23514', CONSTRAINT = 'session_delegation_event_semantics';
            END IF;
        ELSIF NEW.reason_kind IN (
            'parent_stopped_parent_and_descendants',
            'parent_cancelled_parent_and_descendants'
        ) THEN
            IF NEW.provenance_kind <> 'parent_command'
                OR NEW.provenance_session_id <> relation_parent THEN
                RAISE EXCEPTION 'parent disposition has invalid provenance'
                    USING ERRCODE = '23514', CONSTRAINT = 'session_delegation_event_semantics';
            END IF;
            IF relation_policy = 'background' THEN
                expected_outcome := 'continue_running';
            ELSIF NEW.reason_kind = 'parent_stopped_parent_and_descendants' THEN
                expected_outcome := CASE stopped_action
                    WHEN 'keep_running' THEN 'continue_running'
                    WHEN 'stop' THEN 'child_stopped'
                    WHEN 'cancel' THEN 'child_cancelled' END;
            ELSE
                expected_outcome := CASE cancelled_action
                    WHEN 'keep_running' THEN 'continue_running'
                    WHEN 'stop' THEN 'child_stopped'
                    WHEN 'cancel' THEN 'child_cancelled' END;
            END IF;
            IF NEW.outcome_kind <> expected_outcome THEN
                RAISE EXCEPTION 'parent disposition contradicts relationship policy'
                    USING ERRCODE = '23514', CONSTRAINT = 'session_delegation_event_semantics';
            END IF;
        ELSE
            RAISE EXCEPTION 'outcome reason is not a delegation descendant event'
                USING ERRCODE = '23514', CONSTRAINT = 'session_delegation_event_semantics';
        END IF;
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER session_delegation_event_requires_payload
AFTER INSERT ON session_delegation_event DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION require_session_delegation_event_payload();

CREATE TRIGGER session_delegation_is_append_only
BEFORE UPDATE OR DELETE ON session_delegation
FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();
CREATE TRIGGER session_delegation_event_is_append_only
BEFORE UPDATE OR DELETE ON session_delegation_event
FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();
CREATE TRIGGER session_delegation_wait_is_append_only
BEFORE UPDATE OR DELETE ON session_delegation_wait
FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();
CREATE TRIGGER session_message_is_append_only
BEFORE UPDATE OR DELETE ON session_message
FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();
CREATE TRIGGER session_child_result_is_append_only
BEFORE UPDATE OR DELETE ON session_child_result
FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

-- Delegated child creation is request-provenanced rather than a user command.
ALTER TABLE session_model_credential_record
    ALTER COLUMN provenance_command_id DROP NOT NULL,
    ADD COLUMN provenance_tool_request_id uuid REFERENCES tool_request(request_id),
    DROP CONSTRAINT session_model_credential_record_provenance_kind_check,
    DROP CONSTRAINT session_model_credential_record_check;

ALTER TABLE session_model_credential_record
    ADD CONSTRAINT session_model_credential_record_provenance_kind_check CHECK (
        provenance_kind IN ('create_session', 'imported_session', 'migration_backfill',
                            'credential_update', 'delegated_session')
    ),
    ADD CONSTRAINT session_model_credential_record_check CHECK (
        (event_ordinal = 1 AND event_kind = 'created' AND (
            (provenance_kind IN ('create_session', 'imported_session', 'migration_backfill')
                AND provenance_command_id IS NOT NULL AND provenance_tool_request_id IS NULL)
            OR (provenance_kind = 'delegated_session'
                AND provenance_command_id IS NULL AND provenance_tool_request_id IS NOT NULL)))
        OR (event_ordinal > 1 AND event_kind = 'updated'
            AND provenance_kind = 'credential_update'
            AND provenance_command_id IS NOT NULL AND provenance_tool_request_id IS NULL)
    );

CREATE OR REPLACE FUNCTION require_session_creation_command()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE native_count bigint; imported_count bigint; delegated_count bigint;
BEGIN
    SELECT count(*) INTO native_count FROM create_session_command
     WHERE created_session_id = NEW.session_id;
    SELECT count(*) INTO imported_count FROM create_session_from_imported_frontier_command
     WHERE created_session_id = NEW.session_id;
    SELECT count(*) INTO delegated_count FROM session_delegation
     WHERE child_session_id = NEW.session_id;
    IF (NEW.creation_cause = 'owner_initiated' AND NEW.ancestry_kind = 'none'
            AND (native_count, imported_count, delegated_count) <> (1, 0, 0))
        OR (NEW.creation_cause = 'owner_initiated' AND NEW.ancestry_kind = 'imported_conversation'
            AND (native_count, imported_count, delegated_count) <> (0, 1, 0))
        OR (NEW.creation_cause = 'delegated'
            AND (native_count, imported_count, delegated_count) <> (0, 0, 1)) THEN
        RAISE EXCEPTION 'session % requires exactly one matching creation family', NEW.session_id
            USING ERRCODE = '23503', CONSTRAINT = 'session_requires_creation_command';
    END IF;
    RETURN NULL;
END;
$$;
