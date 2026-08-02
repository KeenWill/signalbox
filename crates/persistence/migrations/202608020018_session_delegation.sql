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
    UNIQUE (spawning_tool_request_id, parent_turn_id, parent_session_id),
    UNIQUE (spawning_tool_request_id, parent_session_id, child_session_id),
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

-- A delegated root turn is not accepted user input. The origin discriminator
-- keeps that distinction explicit while the nullable accepted-input foreign
-- key continues to prove the pre-existing origin family.
ALTER TABLE turn_lifecycle
    ADD COLUMN origin_kind text NOT NULL DEFAULT 'accepted_input',
    ALTER COLUMN origin_accepted_input_id DROP NOT NULL,
    ADD CONSTRAINT turn_lifecycle_origin_kind_closed CHECK (
        (origin_kind = 'accepted_input' AND origin_accepted_input_id IS NOT NULL)
        OR (origin_kind = 'delegation' AND origin_accepted_input_id IS NULL)
    ),
    ADD CONSTRAINT turn_lifecycle_delegation_origin_key
        UNIQUE (turn_id, session_id, acceptance_position);

CREATE TABLE session_delegation_initial_task (
    spawning_tool_request_id uuid PRIMARY KEY,
    child_session_id uuid NOT NULL UNIQUE,
    turn_id uuid NOT NULL UNIQUE,
    admission_position numeric(20, 0) NOT NULL CHECK (admission_position = 1),
    defaults_version numeric(20, 0) NOT NULL CHECK (defaults_version = 1),
    frozen_direct_model_selection_id uuid NOT NULL,
    task_content text NOT NULL CHECK (
        task_content <> '' AND octet_length(task_content) <= 1048576
    ),
    UNIQUE (turn_id, child_session_id, admission_position),
    FOREIGN KEY (spawning_tool_request_id, child_session_id)
        REFERENCES session_delegation(spawning_tool_request_id, child_session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (turn_id, child_session_id, admission_position)
        REFERENCES turn_lifecycle(turn_id, session_id, acceptance_position)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (child_session_id, defaults_version)
        REFERENCES session_defaults_version(session_id, version)
        ON UPDATE RESTRICT ON DELETE RESTRICT
);

CREATE FUNCTION require_delegation_initial_task_origin()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF (NEW.origin_kind = 'delegation') <> EXISTS (
        SELECT 1 FROM session_delegation_initial_task AS task
         WHERE task.turn_id = NEW.turn_id
           AND task.child_session_id = NEW.session_id
           AND task.admission_position = NEW.acceptance_position
    ) THEN
        RAISE EXCEPTION 'turn lifecycle requires exactly its typed origin'
            USING ERRCODE = '23514',
                CONSTRAINT = 'turn_lifecycle_typed_origin';
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER turn_lifecycle_requires_typed_origin
AFTER INSERT OR UPDATE ON turn_lifecycle
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION require_delegation_initial_task_origin();

CREATE FUNCTION reject_turn_lifecycle_origin_kind_change()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.origin_kind <> NEW.origin_kind THEN
        RAISE EXCEPTION 'turn lifecycle origin kind is immutable'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER turn_lifecycle_origin_kind_is_immutable
BEFORE UPDATE ON turn_lifecycle
FOR EACH ROW EXECUTE FUNCTION reject_turn_lifecycle_origin_kind_change();

CREATE FUNCTION require_delegation_initial_task_purpose()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM session_delegation AS relation
          JOIN tool_request AS request
            ON request.request_id = relation.spawning_tool_request_id
           AND request.turn_id = relation.parent_turn_id
           AND request.session_id = relation.parent_session_id
          JOIN LATERAL turn_origin_effective_model_configuration(
                relation.parent_turn_id, relation.parent_session_id
          ) AS frozen ON true
         WHERE relation.spawning_tool_request_id = NEW.spawning_tool_request_id
           AND relation.child_session_id = NEW.child_session_id
           AND request.tool_name = 'spawn_session'
           AND request.arguments_kind = 'json'
           AND request.arguments_text::jsonb = jsonb_build_object(
                'relationship', CASE relation.policy_kind
                    WHEN 'background' THEN jsonb_build_object('kind', 'background')
                    WHEN 'bound' THEN jsonb_build_object(
                        'kind', 'bound',
                        'on_parent_stopped', relation.on_parent_stopped,
                        'on_parent_cancelled', relation.on_parent_cancelled
                    )
                END,
                'task', NEW.task_content
           )
           AND frozen.direct_selection_id = NEW.frozen_direct_model_selection_id
    ) THEN
        RAISE EXCEPTION 'delegation initial task contradicts its spawn request'
            USING ERRCODE = '23514',
                CONSTRAINT = 'session_delegation_initial_task_purpose';
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER session_delegation_initial_task_purpose
AFTER INSERT ON session_delegation_initial_task
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION require_delegation_initial_task_purpose();

CREATE TRIGGER session_delegation_initial_task_is_append_only
BEFORE UPDATE OR DELETE ON session_delegation_initial_task
FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE TABLE session_delegation_wait (
    awaiting_tool_request_id uuid PRIMARY KEY,
    spawning_tool_request_id uuid NOT NULL,
    parent_session_id uuid NOT NULL,
    parent_turn_id uuid NOT NULL,
    child_session_id uuid NOT NULL,
    wait_mode text NOT NULL CHECK (wait_mode IN ('foreground', 'background')),
    CHECK (awaiting_tool_request_id <> spawning_tool_request_id),
    UNIQUE (awaiting_tool_request_id, spawning_tool_request_id),
    FOREIGN KEY (awaiting_tool_request_id, parent_turn_id, parent_session_id)
        REFERENCES tool_request(request_id, turn_id, session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (spawning_tool_request_id, parent_session_id, child_session_id)
        REFERENCES session_delegation(
            spawning_tool_request_id, parent_session_id, child_session_id
        )
        ON UPDATE RESTRICT ON DELETE RESTRICT
);

CREATE INDEX session_delegation_wait_by_relation
    ON session_delegation_wait(spawning_tool_request_id, awaiting_tool_request_id);

-- Scheduling writes one proof row only after the exact parent termination
-- command applied with descendant scope. Delegation outcomes consume this row
-- rather than treating an arbitrary parent command as authority.
ALTER TABLE goal_command
    ADD COLUMN descendant_scope text NOT NULL DEFAULT 'parent_alone'
        CHECK (descendant_scope IN ('parent_alone', 'parent_and_descendants')),
    ADD CHECK (operation_kind = 'stop' OR descendant_scope = 'parent_alone');
ALTER TABLE submit_input_command
    ADD COLUMN descendant_scope text NOT NULL DEFAULT 'parent_alone'
        CHECK (descendant_scope IN ('parent_alone', 'parent_and_descendants')),
    ADD CHECK (delivery_kind = 'interrupt' OR descendant_scope = 'parent_alone');
ALTER TABLE accepted_input
    ADD COLUMN descendant_scope text NOT NULL DEFAULT 'parent_alone'
        CHECK (descendant_scope IN ('parent_alone', 'parent_and_descendants')),
    ADD CHECK (delivery_kind = 'interrupt' OR descendant_scope = 'parent_alone');
ALTER TABLE submit_input_command
    ADD CONSTRAINT submit_input_command_scope_key
        UNIQUE (command_id, descendant_scope);
ALTER TABLE accepted_input
    ADD CONSTRAINT accepted_input_command_scope_fk
        FOREIGN KEY (accepting_command_id, descendant_scope)
        REFERENCES submit_input_command(command_id, descendant_scope)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;

CREATE TABLE session_delegation_termination_cascade (
    root_command_id uuid PRIMARY KEY,
    root_session_id uuid NOT NULL,
    root_turn_id uuid NOT NULL,
    termination_kind text NOT NULL CHECK (
        termination_kind IN ('stopped', 'cancelled')
    ),
    descendant_scope text NOT NULL CHECK (descendant_scope = 'parent_and_descendants'),
    disposition_count numeric(20, 0) NOT NULL CHECK (
        disposition_count BETWEEN 0 AND 18446744073709551615
    ),
    FOREIGN KEY (root_command_id) REFERENCES durable_command(command_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (root_turn_id, root_session_id)
        REFERENCES turn_lifecycle(turn_id, session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED
);

CREATE TABLE session_delegation_parent_termination (
    spawning_tool_request_id uuid PRIMARY KEY,
    root_command_id uuid NOT NULL,
    parent_session_id uuid NOT NULL,
    parent_turn_id uuid NOT NULL,
    termination_kind text NOT NULL CHECK (termination_kind IN ('stopped', 'cancelled')),
    source_kind text NOT NULL CHECK (source_kind IN ('root', 'parent_disposition')),
    source_spawning_tool_request_id uuid,
    UNIQUE (spawning_tool_request_id, root_command_id),
    CONSTRAINT session_delegation_parent_termination_source_shape CHECK (
        (source_kind = 'root' AND source_spawning_tool_request_id IS NULL)
        OR (source_kind = 'parent_disposition'
            AND source_spawning_tool_request_id IS NOT NULL)
    ),
    FOREIGN KEY (root_command_id)
        REFERENCES session_delegation_termination_cascade(root_command_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (source_spawning_tool_request_id, root_command_id)
        REFERENCES session_delegation_parent_termination(
            spawning_tool_request_id, root_command_id
        )
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (spawning_tool_request_id, parent_session_id)
        REFERENCES session_delegation(
            spawning_tool_request_id, parent_session_id
        )
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (parent_turn_id, parent_session_id)
        REFERENCES turn_lifecycle(turn_id, session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED
);
CREATE INDEX session_delegation_termination_by_root
    ON session_delegation_parent_termination(
        root_command_id, spawning_tool_request_id
    );

CREATE FUNCTION require_delegation_termination_cascade_command()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF (NEW.termination_kind = 'stopped' AND NOT EXISTS (
            SELECT 1 FROM goal_command
             WHERE command_id = NEW.root_command_id
               AND session_id = NEW.root_session_id
               AND operation_kind = 'stop' AND result_kind = 'applied'
               AND descendant_scope = NEW.descendant_scope))
        OR (NEW.termination_kind = 'cancelled' AND NOT EXISTS (
            SELECT 1 FROM submit_input_command
             WHERE command_id = NEW.root_command_id
               AND session_id = NEW.root_session_id
               AND delivery_kind = 'interrupt'
               AND expected_active_turn_id = NEW.root_turn_id
               AND result_kind = 'applied' AND rejection_kind IS NULL
               AND descendant_scope = NEW.descendant_scope)) THEN
        RAISE EXCEPTION 'delegation cascade lacks its exact applied root command'
            USING ERRCODE = '23514',
                CONSTRAINT = 'session_delegation_termination_cascade_command';
    END IF;
    RETURN NULL;
END;
$$;

CREATE FUNCTION require_delegation_parent_termination_chain()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    cascade session_delegation_termination_cascade%ROWTYPE;
BEGIN
    SELECT * INTO cascade FROM session_delegation_termination_cascade
     WHERE root_command_id = NEW.root_command_id;
    IF cascade.root_command_id IS NULL THEN
        RETURN NULL;
    END IF;
    IF NEW.source_kind = 'root' THEN
        IF NEW.parent_session_id <> cascade.root_session_id
            OR NEW.parent_turn_id <> cascade.root_turn_id
            OR NEW.termination_kind <> cascade.termination_kind THEN
            RAISE EXCEPTION 'direct delegation termination contradicts its root command'
                USING ERRCODE = '23514',
                    CONSTRAINT = 'session_delegation_parent_termination_chain';
        END IF;
    ELSIF NOT EXISTS (
        SELECT 1
          FROM session_delegation_event AS source_event
          JOIN session_delegation AS source_relation
            ON source_relation.spawning_tool_request_id =
                source_event.spawning_tool_request_id
          JOIN session_delegation_initial_task AS source_task
            ON source_task.spawning_tool_request_id =
                source_event.spawning_tool_request_id
           AND source_task.child_session_id = source_relation.child_session_id
          JOIN session_delegation_parent_termination AS source_authority
            ON source_authority.spawning_tool_request_id =
                source_event.spawning_tool_request_id
           AND source_authority.root_command_id = NEW.root_command_id
         WHERE source_event.spawning_tool_request_id =
                NEW.source_spawning_tool_request_id
           AND source_event.event_kind = 'outcome_recorded'
           AND source_event.outcome_kind IN ('child_stopped', 'child_cancelled')
           AND source_event.provenance_kind = 'parent_command'
           AND source_event.provenance_command_id = NEW.root_command_id
           AND source_relation.child_session_id = NEW.parent_session_id
           AND source_task.turn_id = NEW.parent_turn_id
           AND NEW.termination_kind = CASE source_event.outcome_kind
                WHEN 'child_stopped' THEN 'stopped'
                WHEN 'child_cancelled' THEN 'cancelled'
           END
    ) THEN
        RAISE EXCEPTION 'nested delegation termination lacks its immediate parent outcome'
            USING ERRCODE = '23514',
                CONSTRAINT = 'session_delegation_parent_termination_chain';
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER session_delegation_termination_cascade_command
AFTER INSERT ON session_delegation_termination_cascade
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION require_delegation_termination_cascade_command();

CREATE CONSTRAINT TRIGGER session_delegation_parent_termination_chain
AFTER INSERT ON session_delegation_parent_termination
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION require_delegation_parent_termination_chain();

CREATE FUNCTION require_delegation_cascade_disposition_count()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    checked_command uuid := COALESCE(NEW.root_command_id, OLD.root_command_id);
    expected_count numeric(20, 0);
BEGIN
    SELECT disposition_count INTO expected_count
      FROM session_delegation_termination_cascade
     WHERE root_command_id = checked_command;
    IF expected_count IS NOT NULL AND expected_count <> (
        SELECT count(*) FROM session_delegation_parent_termination
         WHERE root_command_id = checked_command
    ) THEN
        RAISE EXCEPTION 'delegation cascade disposition count is incomplete'
            USING ERRCODE = '23514',
                CONSTRAINT = 'session_delegation_cascade_disposition_count';
    END IF;
    RETURN NULL;
END;
$$;
CREATE CONSTRAINT TRIGGER delegation_cascade_requires_disposition_count
AFTER INSERT OR UPDATE ON session_delegation_termination_cascade
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION require_delegation_cascade_disposition_count();
CREATE CONSTRAINT TRIGGER delegation_disposition_requires_cascade_count
AFTER INSERT OR UPDATE OR DELETE ON session_delegation_parent_termination
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION require_delegation_cascade_disposition_count();

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
            'child_completed', 'child_execution_failed', 'child_result_unavailable',
            'child_cancelled',
            'parent_stopped_parent_and_descendants',
            'parent_cancelled_parent_and_descendants'
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
    CONSTRAINT session_delegation_spawn_ordinal CHECK (
        (event_kind = 'spawned') = (event_ordinal = 1)
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

CREATE UNIQUE INDEX session_delegation_spawn_once
    ON session_delegation_event(spawning_tool_request_id)
    WHERE event_kind = 'spawned';
CREATE UNIQUE INDEX session_delegation_message_request_once
    ON session_delegation_event(provenance_tool_request_id)
    WHERE event_kind = 'message_delivered';
CREATE UNIQUE INDEX session_delegation_child_outcome_authority_once
    ON session_delegation_event(
        spawning_tool_request_id, provenance_session_id, provenance_turn_id
    ) WHERE event_kind = 'outcome_recorded' AND provenance_kind = 'child_turn';
CREATE UNIQUE INDEX session_delegation_parent_outcome_authority_once
    ON session_delegation_event(spawning_tool_request_id, provenance_command_id)
    WHERE event_kind = 'outcome_recorded' AND provenance_kind = 'parent_command';

CREATE FUNCTION require_delegation_spawn_history()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF (SELECT count(*) FROM session_delegation_event
         WHERE spawning_tool_request_id = NEW.spawning_tool_request_id
           AND event_ordinal = 1 AND event_kind = 'spawned') <> 1 THEN
        RAISE EXCEPTION 'delegation requires exactly one ordinal-one spawn event'
            USING ERRCODE = '23503',
                CONSTRAINT = 'session_delegation_spawn_history';
    END IF;
    IF (SELECT count(*)
          FROM session_delegation_initial_task AS task
          JOIN turn_lifecycle AS lifecycle
            ON lifecycle.turn_id = task.turn_id
           AND lifecycle.session_id = task.child_session_id
           AND lifecycle.acceptance_position = task.admission_position
         WHERE task.spawning_tool_request_id = NEW.spawning_tool_request_id
           AND task.child_session_id = NEW.child_session_id
           AND task.admission_position = 1
           AND lifecycle.origin_kind = 'delegation'
           AND lifecycle.origin_accepted_input_id IS NULL) <> 1 THEN
        RAISE EXCEPTION 'delegation requires exactly one typed initial task turn'
            USING ERRCODE = '23503',
                CONSTRAINT = 'session_delegation_initial_task_history';
    END IF;
    RETURN NULL;
END;
$$;
CREATE CONSTRAINT TRIGGER session_delegation_requires_spawn_history
AFTER INSERT ON session_delegation DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION require_delegation_spawn_history();

CREATE FUNCTION require_initial_task_relation_history()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    checked_request uuid := COALESCE(NEW.spawning_tool_request_id, OLD.spawning_tool_request_id);
    relation session_delegation%ROWTYPE;
BEGIN
    SELECT * INTO relation FROM session_delegation
     WHERE spawning_tool_request_id = checked_request;
    IF relation.spawning_tool_request_id IS NOT NULL
        AND (SELECT count(*)
              FROM session_delegation_initial_task AS task
              JOIN turn_lifecycle AS lifecycle
                ON lifecycle.turn_id = task.turn_id
               AND lifecycle.session_id = task.child_session_id
               AND lifecycle.acceptance_position = task.admission_position
             WHERE task.spawning_tool_request_id = checked_request
               AND task.child_session_id = relation.child_session_id
               AND task.admission_position = 1
               AND lifecycle.origin_kind = 'delegation'
               AND lifecycle.origin_accepted_input_id IS NULL) <> 1 THEN
        RAISE EXCEPTION 'delegation relation lost its typed initial task turn'
            USING ERRCODE = '23503',
                CONSTRAINT = 'session_delegation_initial_task_history';
    END IF;
    RETURN NULL;
END;
$$;
CREATE CONSTRAINT TRIGGER initial_task_requires_relation_history
AFTER INSERT OR UPDATE OR DELETE ON session_delegation_initial_task
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION require_initial_task_relation_history();

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
    stored_content text;
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
              AND tool_name = 'spawn_session'
              AND arguments_kind = 'json')
    ) THEN
        RAISE EXCEPTION 'spawn provenance does not match delegation parent'
            USING ERRCODE = '23514', CONSTRAINT = 'session_delegation_event_semantics';
    ELSIF NEW.event_kind = 'message_delivered' THEN
        SELECT direction, content_text INTO stored_direction, stored_content
          FROM session_message
         WHERE spawning_tool_request_id = NEW.spawning_tool_request_id
           AND event_ordinal = NEW.event_ordinal;
        IF NEW.provenance_kind <> 'tool_request'
            OR NOT EXISTS (SELECT 1 FROM tool_request
                WHERE request_id = NEW.provenance_tool_request_id
                  AND tool_name = 'send_session_message'
                  AND arguments_kind = 'json'
                  AND arguments_text::jsonb = jsonb_build_object(
                      'content', stored_content,
                      'peer_session_id', CASE stored_direction
                          WHEN 'parent_to_child' THEN relation_child::text
                          WHEN 'child_to_parent' THEN relation_parent::text
                      END
                  ))
            OR (stored_direction = 'parent_to_child'
                AND NEW.provenance_session_id <> relation_parent)
            OR (stored_direction = 'child_to_parent'
                AND NEW.provenance_session_id <> relation_child) THEN
            RAISE EXCEPTION 'message direction does not match delegation provenance'
                USING ERRCODE = '23514', CONSTRAINT = 'session_delegation_event_semantics';
        END IF;
    ELSIF NEW.event_kind = 'outcome_recorded' THEN
        IF NEW.provenance_kind = 'child_turn' AND NOT EXISTS (
            SELECT 1 FROM session_delegation_initial_task AS task
             WHERE task.spawning_tool_request_id = NEW.spawning_tool_request_id
               AND task.child_session_id = relation_child
               AND task.turn_id = NEW.provenance_turn_id
        ) THEN
            RAISE EXCEPTION 'child outcome does not name the delegated initial task turn'
                USING ERRCODE = '23514', CONSTRAINT = 'session_delegation_event_semantics';
        END IF;
        IF NEW.reason_kind = 'child_completed' THEN
            IF NEW.outcome_kind <> 'result_returned'
                OR NEW.provenance_kind <> 'child_turn'
                OR NEW.provenance_session_id <> relation_child
                OR NOT EXISTS (
                    SELECT 1 FROM turn_lifecycle AS lifecycle
                     WHERE lifecycle.turn_id = NEW.provenance_turn_id
                       AND lifecycle.session_id = relation_child
                       AND lifecycle.state_kind = 'terminal'
                       AND lifecycle.terminal_disposition_kind = 'completed'
                )
                OR NOT EXISTS (
                    SELECT 1 FROM session_child_result AS result
                     WHERE result.spawning_tool_request_id = NEW.spawning_tool_request_id
                       AND result.event_ordinal = NEW.event_ordinal
                       AND result.outcome_kind = 'result_returned'
                       AND result.content_text = (
                            SELECT string_agg(
                                entry.assistant_text_value, ''
                                ORDER BY member.member_position
                            )
                              FROM turn_lifecycle AS lifecycle
                              JOIN context_frontier_member AS member
                                ON member.owning_session_id = lifecycle.session_id
                               AND member.context_frontier_id = lifecycle.terminal_frontier_id
                              JOIN semantic_transcript_entry AS entry
                                ON entry.source_session_id = member.source_session_id
                               AND entry.semantic_entry_id = member.semantic_entry_id
                             WHERE lifecycle.turn_id = NEW.provenance_turn_id
                               AND lifecycle.session_id = relation_child
                               AND entry.payload_kind = 'assistant_text'
                               AND entry.producing_model_call_id =
                                   lifecycle.terminal_model_call_id
                       )
                ) THEN
                RAISE EXCEPTION 'child completion has invalid provenance or outcome'
                    USING ERRCODE = '23514', CONSTRAINT = 'session_delegation_event_semantics';
            END IF;
        ELSIF NEW.reason_kind = 'child_execution_failed' THEN
            IF NEW.outcome_kind <> 'child_failed'
                OR NEW.provenance_kind <> 'child_turn'
                OR NEW.provenance_session_id <> relation_child
                OR NOT EXISTS (
                    SELECT 1 FROM turn_lifecycle
                     WHERE turn_id = NEW.provenance_turn_id
                       AND session_id = relation_child
                       AND state_kind = 'terminal'
                       AND terminal_disposition_kind IN ('failed', 'refused')
                ) THEN
                RAISE EXCEPTION 'child failure has invalid provenance or outcome'
                    USING ERRCODE = '23514', CONSTRAINT = 'session_delegation_event_semantics';
            END IF;
        ELSIF NEW.reason_kind = 'child_result_unavailable' THEN
            IF NEW.outcome_kind <> 'child_failed'
                OR NEW.provenance_kind <> 'child_turn'
                OR NEW.provenance_session_id <> relation_child
                OR NOT EXISTS (
                    SELECT 1 FROM turn_lifecycle AS lifecycle
                     WHERE lifecycle.turn_id = NEW.provenance_turn_id
                       AND lifecycle.session_id = relation_child
                       AND lifecycle.state_kind = 'terminal'
                       AND lifecycle.terminal_disposition_kind = 'completed'
                       AND ((
                            SELECT octet_length(string_agg(
                                entry.assistant_text_value, ''
                                ORDER BY member.member_position
                            ))
                              FROM context_frontier_member AS member
                              JOIN semantic_transcript_entry AS entry
                                ON entry.source_session_id = member.source_session_id
                               AND entry.semantic_entry_id = member.semantic_entry_id
                             WHERE member.owning_session_id = lifecycle.session_id
                               AND member.context_frontier_id = lifecycle.terminal_frontier_id
                               AND entry.payload_kind = 'assistant_text'
                               AND entry.producing_model_call_id =
                                   lifecycle.terminal_model_call_id
                       ) IS NULL OR (
                            SELECT octet_length(string_agg(
                                entry.assistant_text_value, ''
                                ORDER BY member.member_position
                            ))
                              FROM context_frontier_member AS member
                              JOIN semantic_transcript_entry AS entry
                                ON entry.source_session_id = member.source_session_id
                               AND entry.semantic_entry_id = member.semantic_entry_id
                             WHERE member.owning_session_id = lifecycle.session_id
                               AND member.context_frontier_id = lifecycle.terminal_frontier_id
                               AND entry.payload_kind = 'assistant_text'
                               AND entry.producing_model_call_id =
                                   lifecycle.terminal_model_call_id
                       ) > 1048576)
                ) THEN
                RAISE EXCEPTION 'unavailable child result has invalid terminal evidence'
                    USING ERRCODE = '23514', CONSTRAINT = 'session_delegation_event_semantics';
            END IF;
        ELSIF NEW.reason_kind = 'child_cancelled' THEN
            IF NEW.outcome_kind <> 'child_cancelled'
                OR NEW.provenance_kind <> 'child_turn'
                OR NEW.provenance_session_id <> relation_child
                OR NOT EXISTS (
                    SELECT 1 FROM turn_lifecycle
                     WHERE turn_id = NEW.provenance_turn_id
                       AND session_id = relation_child
                       AND state_kind = 'terminal'
                       AND terminal_disposition_kind = 'cancelled'
                ) THEN
                RAISE EXCEPTION 'child cancellation has invalid terminal evidence'
                    USING ERRCODE = '23514', CONSTRAINT = 'session_delegation_event_semantics';
            END IF;
        ELSIF NEW.reason_kind IN (
            'parent_stopped_parent_and_descendants',
            'parent_cancelled_parent_and_descendants'
        ) THEN
            IF NEW.provenance_kind <> 'parent_command'
                OR NEW.provenance_session_id <> relation_parent
                OR NOT EXISTS (
                    SELECT 1 FROM session_delegation_parent_termination AS authority
                     WHERE authority.spawning_tool_request_id =
                            NEW.spawning_tool_request_id
                       AND authority.root_command_id = NEW.provenance_command_id
                       AND authority.parent_session_id = relation_parent
                       AND authority.parent_turn_id = NEW.provenance_turn_id
                       AND authority.termination_kind = CASE NEW.reason_kind
                            WHEN 'parent_stopped_parent_and_descendants' THEN 'stopped'
                            WHEN 'parent_cancelled_parent_and_descendants' THEN 'cancelled'
                       END
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
          AND session_id = NEW.parent_session_id
          AND turn_id = NEW.parent_turn_id
          AND tool_name = 'await_session'
          AND arguments_kind = 'json'
          AND arguments_text::jsonb = jsonb_build_object(
              'child_session_id', NEW.child_session_id::text,
              'mode', NEW.wait_mode
          )) THEN
        RAISE EXCEPTION 'delegation wait requires exact normalized await_session purpose'
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
        'delegation_update', 'delegation_wake')
);
CREATE TABLE delegation_update_outbox_event (
    event_sequence numeric(20, 0) PRIMARY KEY,
    event_kind text NOT NULL CHECK (event_kind = 'delegation_update'),
    storage_version smallint NOT NULL CHECK (storage_version = 1),
    session_id uuid NOT NULL,
    update_kind text NOT NULL CHECK (update_kind IN (
        'child_spawned', 'child_waiting', 'child_lifecycle_disposition',
        'child_result', 'session_message'
    )),
    spawning_tool_request_id uuid NOT NULL,
    child_session_id uuid,
    policy_kind text CHECK (
        policy_kind IS NULL OR policy_kind IN ('background', 'bound')
    ),
    on_parent_stopped text CHECK (
        on_parent_stopped IS NULL OR on_parent_stopped IN ('keep_running', 'stop', 'cancel')
    ),
    on_parent_cancelled text CHECK (
        on_parent_cancelled IS NULL OR on_parent_cancelled IN ('keep_running', 'stop', 'cancel')
    ),
    awaiting_tool_request_id uuid,
    wait_mode text CHECK (
        wait_mode IS NULL OR wait_mode IN ('foreground', 'background')
    ),
    delegation_event_ordinal numeric(20, 0),
    delegation_event_kind text,
    outcome_kind text CHECK (
        outcome_kind IS NULL OR outcome_kind IN (
            'result_returned', 'child_failed', 'child_stopped',
            'child_cancelled', 'continue_running'
        )
    ),
    reason_kind text CHECK (
        reason_kind IS NULL OR reason_kind IN (
            'child_completed', 'child_execution_failed', 'child_result_unavailable',
            'child_cancelled', 'parent_stopped_parent_and_descendants',
            'parent_cancelled_parent_and_descendants'
        )
    ),
    provenance_kind text CHECK (
        provenance_kind IS NULL OR provenance_kind IN ('child_turn', 'parent_command')
    ),
    provenance_session_id uuid,
    provenance_turn_id uuid,
    provenance_command_id uuid,
    result_spawning_request_id uuid,
    message_id uuid,
    sender_session_id uuid,
    recipient_session_id uuid,
    message_ordinal numeric(20, 0) CHECK (
        message_ordinal IS NULL
        OR message_ordinal BETWEEN 1 AND 18446744073709551615
    ),
    content_text text CHECK (
        content_text IS NULL OR octet_length(content_text) BETWEEN 1 AND 1048576
    ),
    CONSTRAINT delegation_update_provenance_shape CHECK (
        (provenance_kind IS NULL AND provenance_session_id IS NULL
            AND provenance_turn_id IS NULL AND provenance_command_id IS NULL)
        OR (provenance_kind IS NOT NULL AND provenance_session_id IS NOT NULL
            AND provenance_turn_id IS NOT NULL
            AND ((provenance_kind = 'child_turn' AND provenance_command_id IS NULL)
                OR (provenance_kind = 'parent_command'
                    AND provenance_command_id IS NOT NULL)))
    ),
    CONSTRAINT delegation_update_subject_shape CHECK (
        (update_kind = 'child_spawned'
            AND child_session_id IS NOT NULL
            AND policy_kind IS NOT NULL
            AND ((policy_kind = 'background'
                    AND on_parent_stopped IS NULL AND on_parent_cancelled IS NULL)
                OR (policy_kind = 'bound'
                    AND on_parent_stopped IS NOT NULL
                    AND on_parent_cancelled IS NOT NULL))
            AND awaiting_tool_request_id IS NULL
            AND wait_mode IS NULL
            AND delegation_event_ordinal IS NOT NULL AND delegation_event_ordinal = 1
            AND delegation_event_kind IS NOT NULL AND delegation_event_kind = 'spawned'
            AND outcome_kind IS NULL AND reason_kind IS NULL
            AND provenance_kind IS NULL
            AND result_spawning_request_id IS NULL
            AND message_id IS NULL AND sender_session_id IS NULL
            AND recipient_session_id IS NULL AND message_ordinal IS NULL
            AND content_text IS NULL)
        OR (update_kind = 'child_waiting'
            AND child_session_id IS NOT NULL
            AND policy_kind IS NULL
            AND on_parent_stopped IS NULL AND on_parent_cancelled IS NULL
            AND awaiting_tool_request_id IS NOT NULL
            AND wait_mode IS NOT NULL
            AND delegation_event_ordinal IS NULL
            AND delegation_event_kind IS NULL
            AND outcome_kind IS NULL AND reason_kind IS NULL
            AND provenance_kind IS NULL
            AND result_spawning_request_id IS NULL
            AND message_id IS NULL AND sender_session_id IS NULL
            AND recipient_session_id IS NULL AND message_ordinal IS NULL
            AND content_text IS NULL)
        OR (update_kind = 'child_lifecycle_disposition'
            AND child_session_id IS NOT NULL
            AND policy_kind IS NULL
            AND on_parent_stopped IS NULL AND on_parent_cancelled IS NULL
            AND awaiting_tool_request_id IS NULL AND wait_mode IS NULL
            AND delegation_event_ordinal IS NOT NULL
            AND delegation_event_kind IS NOT NULL AND delegation_event_kind = 'outcome_recorded'
            AND outcome_kind IS NOT NULL AND reason_kind IS NOT NULL
            AND provenance_kind IS NOT NULL
            AND result_spawning_request_id IS NULL
            AND message_id IS NULL AND sender_session_id IS NULL
            AND recipient_session_id IS NULL AND message_ordinal IS NULL
            AND content_text IS NULL)
        OR (update_kind = 'child_result'
            AND child_session_id IS NOT NULL
            AND policy_kind IS NULL
            AND on_parent_stopped IS NULL AND on_parent_cancelled IS NULL
            AND awaiting_tool_request_id IS NULL AND wait_mode IS NULL
            AND delegation_event_ordinal IS NULL
            AND delegation_event_kind IS NULL
            AND outcome_kind IS NOT NULL AND outcome_kind IN (
                'result_returned', 'child_failed', 'child_stopped', 'child_cancelled'
            )
            AND reason_kind IS NOT NULL AND provenance_kind IS NOT NULL
            AND result_spawning_request_id IS NOT NULL AND result_spawning_request_id = spawning_tool_request_id
            AND message_id IS NULL AND sender_session_id IS NULL
            AND recipient_session_id IS NULL AND message_ordinal IS NULL
            AND ((outcome_kind = 'result_returned' AND content_text IS NOT NULL)
                OR (outcome_kind <> 'result_returned' AND content_text IS NULL)))
        OR (update_kind = 'session_message'
            AND child_session_id IS NULL
            AND policy_kind IS NULL
            AND on_parent_stopped IS NULL AND on_parent_cancelled IS NULL
            AND awaiting_tool_request_id IS NULL
            AND wait_mode IS NULL
            AND delegation_event_ordinal IS NULL
            AND delegation_event_kind IS NULL
            AND outcome_kind IS NULL AND reason_kind IS NULL
            AND provenance_kind IS NULL
            AND result_spawning_request_id IS NULL
            AND message_id IS NOT NULL
            AND sender_session_id IS NOT NULL
            AND recipient_session_id IS NOT NULL
            AND sender_session_id <> recipient_session_id
            AND message_ordinal IS NOT NULL
            AND content_text IS NOT NULL)
    ),
    FOREIGN KEY (event_sequence, event_kind, storage_version, session_id)
        REFERENCES outbox_event(event_sequence, event_kind, storage_version, session_id)
        ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (spawning_tool_request_id)
        REFERENCES session_delegation(spawning_tool_request_id)
        ON DELETE RESTRICT,
    FOREIGN KEY (spawning_tool_request_id, child_session_id)
        REFERENCES session_delegation(spawning_tool_request_id, child_session_id)
        ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (
        spawning_tool_request_id,
        delegation_event_ordinal,
        delegation_event_kind
    ) REFERENCES session_delegation_event(
        spawning_tool_request_id, event_ordinal, event_kind
    ) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (awaiting_tool_request_id, spawning_tool_request_id)
        REFERENCES session_delegation_wait(
            awaiting_tool_request_id, spawning_tool_request_id
        ) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (result_spawning_request_id)
        REFERENCES session_child_result(spawning_tool_request_id)
        ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (message_id, spawning_tool_request_id)
        REFERENCES session_message(message_id, spawning_tool_request_id)
        ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (sender_session_id) REFERENCES session(session_id)
        ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (recipient_session_id) REFERENCES session(session_id)
        ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (provenance_command_id) REFERENCES durable_command(command_id)
        ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED
);
CREATE UNIQUE INDEX delegation_child_spawned_update_once
    ON delegation_update_outbox_event(session_id, spawning_tool_request_id)
    WHERE update_kind = 'child_spawned';
CREATE UNIQUE INDEX delegation_child_waiting_update_once
    ON delegation_update_outbox_event(session_id, awaiting_tool_request_id)
    WHERE update_kind = 'child_waiting';
CREATE UNIQUE INDEX delegation_lifecycle_update_once
    ON delegation_update_outbox_event(
        session_id, spawning_tool_request_id, delegation_event_ordinal
    ) WHERE update_kind = 'child_lifecycle_disposition';
CREATE UNIQUE INDEX delegation_child_result_update_once
    ON delegation_update_outbox_event(session_id, result_spawning_request_id)
    WHERE update_kind = 'child_result';
CREATE UNIQUE INDEX delegation_session_message_update_once
    ON delegation_update_outbox_event(session_id, message_id)
    WHERE update_kind = 'session_message';
CREATE FUNCTION require_delegation_update_subject()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM session_delegation AS relation
         WHERE relation.spawning_tool_request_id = NEW.spawning_tool_request_id
           AND NEW.session_id IN (parent_session_id, child_session_id)
           AND CASE NEW.update_kind
                WHEN 'child_spawned' THEN
                    NEW.child_session_id = relation.child_session_id
                    AND NEW.policy_kind = relation.policy_kind
                    AND NEW.on_parent_stopped IS NOT DISTINCT FROM relation.on_parent_stopped
                    AND NEW.on_parent_cancelled IS NOT DISTINCT FROM relation.on_parent_cancelled
                WHEN 'child_waiting' THEN EXISTS (
                    SELECT 1 FROM session_delegation_wait AS wait
                     WHERE wait.awaiting_tool_request_id = NEW.awaiting_tool_request_id
                       AND wait.spawning_tool_request_id = NEW.spawning_tool_request_id
                       AND wait.child_session_id = NEW.child_session_id
                       AND wait.wait_mode = NEW.wait_mode
                )
                WHEN 'child_lifecycle_disposition' THEN EXISTS (
                    SELECT 1 FROM session_delegation_event AS event
                     WHERE event.spawning_tool_request_id = NEW.spawning_tool_request_id
                       AND event.event_ordinal = NEW.delegation_event_ordinal
                       AND event.event_kind = 'outcome_recorded'
                       AND event.outcome_kind = NEW.outcome_kind
                       AND event.reason_kind = NEW.reason_kind
                       AND event.provenance_kind = NEW.provenance_kind
                       AND event.provenance_session_id = NEW.provenance_session_id
                       AND event.provenance_turn_id = NEW.provenance_turn_id
                       AND event.provenance_command_id IS NOT DISTINCT FROM
                            NEW.provenance_command_id
                       AND relation.child_session_id = NEW.child_session_id
                )
                WHEN 'child_result' THEN EXISTS (
                    SELECT 1
                      FROM session_child_result AS result
                      JOIN session_delegation_event AS event
                        ON event.spawning_tool_request_id =
                            result.spawning_tool_request_id
                       AND event.event_ordinal = result.event_ordinal
                     WHERE result.spawning_tool_request_id =
                            NEW.result_spawning_request_id
                       AND result.outcome_kind = NEW.outcome_kind
                       AND result.content_text IS NOT DISTINCT FROM NEW.content_text
                       AND event.reason_kind = NEW.reason_kind
                       AND event.provenance_kind = NEW.provenance_kind
                       AND event.provenance_session_id = NEW.provenance_session_id
                       AND event.provenance_turn_id = NEW.provenance_turn_id
                       AND event.provenance_command_id IS NOT DISTINCT FROM
                            NEW.provenance_command_id
                       AND relation.child_session_id = NEW.child_session_id
                )
                WHEN 'session_message' THEN EXISTS (
                    SELECT 1 FROM session_message AS message
                     WHERE message.message_id = NEW.message_id
                       AND message.spawning_tool_request_id =
                            NEW.spawning_tool_request_id
                       AND message.event_ordinal = NEW.message_ordinal
                       AND message.content_text = NEW.content_text
                       AND NEW.sender_session_id = CASE message.direction
                            WHEN 'parent_to_child' THEN relation.parent_session_id
                            WHEN 'child_to_parent' THEN relation.child_session_id
                       END
                       AND NEW.recipient_session_id = CASE message.direction
                            WHEN 'parent_to_child' THEN relation.child_session_id
                            WHEN 'child_to_parent' THEN relation.parent_session_id
                       END
                )
           END
    ) THEN
        RAISE EXCEPTION 'delegation update payload does not match its typed state'
            USING ERRCODE = '23514',
                CONSTRAINT = 'delegation_update_subject';
    END IF;
    RETURN NULL;
END;
$$;
CREATE CONSTRAINT TRIGGER delegation_update_subject
AFTER INSERT ON delegation_update_outbox_event DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION require_delegation_update_subject();
CREATE TRIGGER delegation_update_outbox_event_is_append_only
BEFORE UPDATE OR DELETE ON delegation_update_outbox_event
FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();
CREATE TRIGGER delegation_update_outbox_event_cannot_be_truncated
BEFORE TRUNCATE ON delegation_update_outbox_event
FOR EACH STATEMENT EXECUTE FUNCTION reject_outbox_table_truncate();

CREATE TABLE delegation_wake_outbox_event (
    event_sequence numeric(20, 0) PRIMARY KEY,
    event_kind text NOT NULL CHECK (event_kind = 'delegation_wake'),
    storage_version smallint NOT NULL CHECK (storage_version = 1),
    session_id uuid NOT NULL,
    spawning_tool_request_id uuid NOT NULL,
    subject_kind text NOT NULL CHECK (subject_kind IN ('result', 'message')),
    result_spawning_request_id uuid,
    message_id uuid,
    CONSTRAINT delegation_wake_subject_shape CHECK ((subject_kind = 'result'
            AND result_spawning_request_id IS NOT NULL
            AND result_spawning_request_id = spawning_tool_request_id
            AND message_id IS NULL)
        OR (subject_kind = 'message' AND result_spawning_request_id IS NULL
            AND message_id IS NOT NULL)),
    FOREIGN KEY (event_sequence, event_kind, storage_version, session_id)
        REFERENCES outbox_event(event_sequence, event_kind, storage_version, session_id)
        ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (result_spawning_request_id)
        REFERENCES session_child_result(spawning_tool_request_id)
        ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (result_spawning_request_id, session_id)
        REFERENCES session_delegation(spawning_tool_request_id, parent_session_id)
        ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (message_id, spawning_tool_request_id)
        REFERENCES session_message(message_id, spawning_tool_request_id)
        ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED
);
CREATE UNIQUE INDEX delegation_result_wake_once
    ON delegation_wake_outbox_event(result_spawning_request_id)
    WHERE subject_kind = 'result';
CREATE UNIQUE INDEX delegation_message_wake_once
    ON delegation_wake_outbox_event(message_id)
    WHERE subject_kind = 'message';
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
        WHEN 'delegation_update' THEN SELECT count(*) INTO matching_records FROM delegation_update_outbox_event WHERE event_sequence = NEW.event_sequence;
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
CREATE TRIGGER session_delegation_parent_termination_is_append_only
BEFORE UPDATE OR DELETE ON session_delegation_parent_termination
FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();
CREATE TRIGGER session_delegation_termination_cascade_is_append_only
BEFORE UPDATE OR DELETE ON session_delegation_termination_cascade
FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();
CREATE TRIGGER session_message_is_append_only
BEFORE UPDATE OR DELETE ON session_message
FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();
CREATE TRIGGER session_child_result_is_append_only
BEFORE UPDATE OR DELETE ON session_child_result
FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE FUNCTION reject_session_delegation_table_truncate()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'delegation relations and histories are append-only'
        USING ERRCODE = '23514';
END;
$$;

CREATE TRIGGER session_delegation_cannot_be_truncated
BEFORE TRUNCATE ON session_delegation
FOR EACH STATEMENT EXECUTE FUNCTION reject_session_delegation_table_truncate();
CREATE TRIGGER session_delegation_event_cannot_be_truncated
BEFORE TRUNCATE ON session_delegation_event
FOR EACH STATEMENT EXECUTE FUNCTION reject_session_delegation_table_truncate();
CREATE TRIGGER session_delegation_wait_cannot_be_truncated
BEFORE TRUNCATE ON session_delegation_wait
FOR EACH STATEMENT EXECUTE FUNCTION reject_session_delegation_table_truncate();
CREATE TRIGGER session_delegation_parent_termination_cannot_be_truncated
BEFORE TRUNCATE ON session_delegation_parent_termination
FOR EACH STATEMENT EXECUTE FUNCTION reject_session_delegation_table_truncate();
CREATE TRIGGER session_delegation_termination_cascade_cannot_be_truncated
BEFORE TRUNCATE ON session_delegation_termination_cascade
FOR EACH STATEMENT EXECUTE FUNCTION reject_session_delegation_table_truncate();
CREATE TRIGGER session_delegation_initial_task_cannot_be_truncated
BEFORE TRUNCATE ON session_delegation_initial_task
FOR EACH STATEMENT EXECUTE FUNCTION reject_session_delegation_table_truncate();
CREATE TRIGGER session_message_cannot_be_truncated
BEFORE TRUNCATE ON session_message
FOR EACH STATEMENT EXECUTE FUNCTION reject_session_delegation_table_truncate();
CREATE TRIGGER session_child_result_cannot_be_truncated
BEFORE TRUNCATE ON session_child_result
FOR EACH STATEMENT EXECUTE FUNCTION reject_session_delegation_table_truncate();

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
    ),
    ADD CONSTRAINT delegated_session_credential_relation_fk
        FOREIGN KEY (provenance_tool_request_id, session_id)
        REFERENCES session_delegation(spawning_tool_request_id, child_session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;

CREATE FUNCTION require_delegated_session_credential_purpose()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.provenance_kind = 'delegated_session' AND NOT EXISTS (
        SELECT 1 FROM tool_request
         WHERE request_id = NEW.provenance_tool_request_id
           AND tool_name = 'spawn_session'
    ) THEN
        RAISE EXCEPTION 'delegated credentials require their exact spawn request'
            USING ERRCODE = '23514',
                CONSTRAINT = 'delegated_session_credential_purpose';
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER delegated_session_credential_purpose
AFTER INSERT ON session_model_credential_record
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION require_delegated_session_credential_purpose();

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
