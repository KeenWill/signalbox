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
    UNIQUE (spawning_tool_request_id, parent_session_id),
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
            'child_stopped', 'child_cancelled',
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
        OR (provenance_kind = 'parent_command' AND provenance_turn_id IS NOT NULL
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
    UNIQUE (message_id, spawning_tool_request_id),
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
    IF NEW.event_kind = 'outcome_recorded' AND latest IS NOT NULL AND EXISTS (
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

CREATE FUNCTION durable_command_belongs_to_session(
    checked_command_id uuid,
    checked_session_id uuid
)
RETURNS boolean LANGUAGE sql STABLE AS $$
    SELECT EXISTS (SELECT 1 FROM create_session_command
        WHERE command_id = checked_command_id AND created_session_id = checked_session_id)
    OR EXISTS (SELECT 1 FROM create_session_from_imported_frontier_command
        WHERE command_id = checked_command_id AND created_session_id = checked_session_id)
    OR EXISTS (SELECT 1 FROM replace_session_defaults_command
        WHERE command_id = checked_command_id AND session_id = checked_session_id)
    OR EXISTS (SELECT 1 FROM replace_session_metadata_command
        WHERE command_id = checked_command_id AND session_id = checked_session_id)
    OR EXISTS (SELECT 1 FROM submit_input_command
        WHERE command_id = checked_command_id AND session_id = checked_session_id)
    OR EXISTS (
        SELECT 1 FROM decide_tool_request_command AS command
        JOIN tool_request AS request ON request.request_id = command.request_id
        WHERE command.command_id = checked_command_id
          AND request.session_id = checked_session_id
    )
    OR EXISTS (SELECT 1 FROM compact_session_command
        WHERE command_id = checked_command_id AND session_id = checked_session_id)
    OR EXISTS (SELECT 1 FROM goal_command
        WHERE command_id = checked_command_id AND session_id = checked_session_id)
$$;

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
        AND EXISTS (SELECT 1 FROM tool_request
            WHERE request_id = NEW.spawning_tool_request_id
              AND tool_name = 'spawn_session')
    ) THEN
        RAISE EXCEPTION 'spawn provenance does not match delegation parent'
            USING ERRCODE = '23514', CONSTRAINT = 'session_delegation_event_semantics';
    ELSIF NEW.event_kind = 'message_delivered' THEN
        SELECT direction INTO stored_direction FROM session_message
         WHERE spawning_tool_request_id = NEW.spawning_tool_request_id
           AND event_ordinal = NEW.event_ordinal;
        IF NEW.provenance_kind <> 'tool_request'
            OR NOT EXISTS (SELECT 1 FROM tool_request
                WHERE request_id = NEW.provenance_tool_request_id
                  AND tool_name = 'send_session_message')
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
                OR NEW.provenance_kind <> 'child_turn'
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
        ELSIF NEW.reason_kind IN ('child_stopped', 'child_cancelled') THEN
            IF NEW.outcome_kind <> NEW.reason_kind
                OR NEW.provenance_kind <> 'child_turn'
                OR NEW.provenance_session_id <> relation_child THEN
                RAISE EXCEPTION 'child disposition has invalid provenance or outcome'
                    USING ERRCODE = '23514', CONSTRAINT = 'session_delegation_event_semantics';
            END IF;
        ELSIF NEW.reason_kind IN (
            'parent_stopped_parent_and_descendants',
            'parent_cancelled_parent_and_descendants'
        ) THEN
            IF NEW.provenance_kind <> 'parent_command'
                OR NEW.provenance_session_id <> relation_parent
                OR NEW.provenance_turn_id <> (
                    SELECT parent_turn_id FROM session_delegation
                     WHERE spawning_tool_request_id = NEW.spawning_tool_request_id
                )
                OR NOT durable_command_belongs_to_session(
                    NEW.provenance_command_id, relation_parent
                ) THEN
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

CREATE FUNCTION require_delegation_wait_purpose()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM tool_request
        WHERE request_id = NEW.awaiting_tool_request_id
          AND tool_name = 'await_session') THEN
        RAISE EXCEPTION 'delegation wait requires await_session request'
            USING ERRCODE = '23514', CONSTRAINT = 'session_delegation_wait_purpose';
    END IF;
    RETURN NULL;
END;
$$;
CREATE CONSTRAINT TRIGGER session_delegation_wait_purpose
AFTER INSERT ON session_delegation_wait DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION require_delegation_wait_purpose();

ALTER TABLE outbox_event DROP CONSTRAINT outbox_event_kind_closed;
ALTER TABLE outbox_event ADD CONSTRAINT outbox_event_kind_closed CHECK (
    event_kind IN ('session_created', 'input_accepted', 'goal_turn_retired',
        'turn_activated', 'turn_failed', 'model_call_transition',
        'tool_batch_transition', 'context_compacted', 'turn_completed',
        'turn_refused', 'turn_cancelled', 'turn_reconciliation_required',
        'delegation_wake')
);
CREATE TABLE delegation_wake_outbox_event (
    event_sequence numeric(20, 0) PRIMARY KEY,
    event_kind text NOT NULL CHECK (event_kind = 'delegation_wake'),
    storage_version smallint NOT NULL CHECK (storage_version = 1),
    session_id uuid NOT NULL,
    spawning_tool_request_id uuid NOT NULL,
    subject_kind text NOT NULL CHECK (subject_kind IN ('result', 'message')),
    result_spawning_request_id uuid,
    message_id uuid,
    CHECK ((subject_kind = 'result' AND result_spawning_request_id = spawning_tool_request_id
            AND message_id IS NULL)
        OR (subject_kind = 'message' AND result_spawning_request_id IS NULL
            AND message_id IS NOT NULL)),
    FOREIGN KEY (event_sequence, event_kind, storage_version, session_id)
        REFERENCES outbox_event(event_sequence, event_kind, storage_version, session_id)
        ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (result_spawning_request_id)
        REFERENCES session_child_result(spawning_tool_request_id)
        ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (message_id, spawning_tool_request_id)
        REFERENCES session_message(message_id, spawning_tool_request_id)
        ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED
);
CREATE FUNCTION require_delegation_wake_recipient()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF (NEW.subject_kind = 'result' AND NOT EXISTS (
            SELECT 1 FROM session_delegation WHERE spawning_tool_request_id = NEW.spawning_tool_request_id
              AND parent_session_id = NEW.session_id))
        OR (NEW.subject_kind = 'message' AND NOT EXISTS (
            SELECT 1 FROM session_message AS message JOIN session_delegation AS relation
              ON relation.spawning_tool_request_id = message.spawning_tool_request_id
            WHERE message.message_id = NEW.message_id
              AND ((message.direction = 'parent_to_child' AND relation.child_session_id = NEW.session_id)
                OR (message.direction = 'child_to_parent' AND relation.parent_session_id = NEW.session_id)))) THEN
        RAISE EXCEPTION 'delegation wake recipient does not match its subject'
            USING ERRCODE = '23514', CONSTRAINT = 'delegation_wake_recipient';
    END IF;
    RETURN NULL;
END;
$$;
CREATE CONSTRAINT TRIGGER delegation_wake_recipient
AFTER INSERT ON delegation_wake_outbox_event DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION require_delegation_wake_recipient();
CREATE TRIGGER delegation_wake_outbox_event_is_append_only
BEFORE UPDATE OR DELETE ON delegation_wake_outbox_event
FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();
CREATE TRIGGER delegation_wake_outbox_event_cannot_be_truncated
BEFORE TRUNCATE ON delegation_wake_outbox_event
FOR EACH STATEMENT EXECUTE FUNCTION reject_outbox_table_truncate();
CREATE OR REPLACE FUNCTION require_outbox_event_typed_record()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE matching_records bigint;
BEGIN
    CASE NEW.event_kind
        WHEN 'session_created' THEN SELECT count(*) INTO matching_records FROM session_created_outbox_event WHERE event_sequence = NEW.event_sequence;
        WHEN 'input_accepted' THEN SELECT count(*) INTO matching_records FROM input_accepted_outbox_event WHERE event_sequence = NEW.event_sequence;
        WHEN 'goal_turn_retired' THEN SELECT count(*) INTO matching_records FROM goal_turn_retired_outbox_event WHERE event_sequence = NEW.event_sequence;
        WHEN 'turn_activated' THEN SELECT count(*) INTO matching_records FROM turn_activated_outbox_event WHERE event_sequence = NEW.event_sequence;
        WHEN 'turn_failed' THEN SELECT count(*) INTO matching_records FROM turn_failed_outbox_event WHERE event_sequence = NEW.event_sequence;
        WHEN 'model_call_transition' THEN SELECT count(*) INTO matching_records FROM model_call_transition_outbox_event WHERE event_sequence = NEW.event_sequence;
        WHEN 'tool_batch_transition' THEN SELECT count(*) INTO matching_records FROM tool_batch_transition_outbox_event WHERE event_sequence = NEW.event_sequence;
        WHEN 'context_compacted' THEN SELECT count(*) INTO matching_records FROM context_compacted_outbox_event WHERE event_sequence = NEW.event_sequence;
        WHEN 'turn_completed' THEN SELECT count(*) INTO matching_records FROM turn_completed_outbox_event WHERE event_sequence = NEW.event_sequence;
        WHEN 'turn_refused' THEN SELECT count(*) INTO matching_records FROM turn_refused_outbox_event WHERE event_sequence = NEW.event_sequence;
        WHEN 'turn_cancelled' THEN SELECT count(*) INTO matching_records FROM turn_cancelled_outbox_event WHERE event_sequence = NEW.event_sequence;
        WHEN 'turn_reconciliation_required' THEN SELECT count(*) INTO matching_records FROM turn_reconciliation_required_outbox_event WHERE event_sequence = NEW.event_sequence;
        WHEN 'delegation_wake' THEN SELECT count(*) INTO matching_records FROM delegation_wake_outbox_event WHERE event_sequence = NEW.event_sequence;
        ELSE RAISE EXCEPTION 'unsupported outbox event kind %', NEW.event_kind USING ERRCODE = '23514';
    END CASE;
    IF matching_records <> 1 THEN
        RAISE EXCEPTION 'outbox event % requires exactly one % typed record', NEW.event_sequence, NEW.event_kind USING ERRCODE = '23503';
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
