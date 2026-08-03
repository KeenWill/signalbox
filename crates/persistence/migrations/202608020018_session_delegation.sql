-- Retain the caller-selected descendant scope on both parent termination
-- command families. This migration owns the complete delegated-session
-- persistence surface.

ALTER TABLE goal_command
    ADD COLUMN descendant_scope text;

DROP TRIGGER goal_command_is_append_only ON goal_command;

UPDATE goal_command
   SET descendant_scope = 'parent_alone'
 WHERE operation_kind = 'stop';

CREATE TRIGGER goal_command_is_append_only
    BEFORE UPDATE OR DELETE ON goal_command
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

ALTER TABLE goal_command
    ADD CONSTRAINT goal_command_descendant_scope_shape CHECK (
        (
            operation_kind = 'stop'
            AND descendant_scope IS NOT NULL
            AND descendant_scope IN ('parent_alone', 'parent_and_descendants')
        )
        OR (operation_kind <> 'stop' AND descendant_scope IS NULL)
    );

ALTER TABLE submit_input_command
    ADD COLUMN descendant_scope text;

ALTER TABLE accepted_input
    ADD COLUMN descendant_scope text;

DROP TRIGGER submit_input_command_is_append_only ON submit_input_command;
DROP TRIGGER accepted_input_is_append_only ON accepted_input;

UPDATE submit_input_command
   SET descendant_scope = 'parent_alone'
 WHERE delivery_kind = 'interrupt';

UPDATE accepted_input
   SET descendant_scope = 'parent_alone'
 WHERE delivery_kind = 'interrupt';

CREATE TRIGGER submit_input_command_is_append_only
    BEFORE UPDATE OR DELETE ON submit_input_command
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER accepted_input_is_append_only
    BEFORE UPDATE OR DELETE ON accepted_input
    FOR EACH ROW EXECUTE FUNCTION reject_invalid_accepted_input_change();

CREATE FUNCTION reject_accepted_input_descendant_scope_change()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.descendant_scope IS DISTINCT FROM NEW.descendant_scope THEN
        RAISE EXCEPTION 'accepted input descendant scope is immutable'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER accepted_input_descendant_scope_is_immutable
    BEFORE UPDATE ON accepted_input
    FOR EACH ROW EXECUTE FUNCTION reject_accepted_input_descendant_scope_change();

ALTER TABLE submit_input_command
    ADD CONSTRAINT submit_input_command_descendant_scope_shape CHECK (
        (
            delivery_kind = 'interrupt'
            AND descendant_scope IS NOT NULL
            AND descendant_scope IN ('parent_alone', 'parent_and_descendants')
        )
        OR (delivery_kind <> 'interrupt' AND descendant_scope IS NULL)
    );

ALTER TABLE accepted_input
    ADD CONSTRAINT accepted_input_descendant_scope_shape CHECK (
        (
            delivery_kind = 'interrupt'
            AND descendant_scope IS NOT NULL
            AND descendant_scope IN ('parent_alone', 'parent_and_descendants')
        )
        OR (delivery_kind <> 'interrupt' AND descendant_scope IS NULL)
    );

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

CREATE FUNCTION lock_delegation_parent_for_spawn()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    PERFORM 1 FROM session
     WHERE session_id = NEW.parent_session_id
     FOR UPDATE;
    RETURN NEW;
END;
$$;
CREATE TRIGGER session_delegation_locks_parent_for_spawn
BEFORE INSERT ON session_delegation
FOR EACH ROW EXECUTE FUNCTION lock_delegation_parent_for_spawn();

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
    semantic_entry_id uuid NOT NULL UNIQUE,
    admission_position numeric(20, 0) NOT NULL CHECK (admission_position = 1),
    defaults_version numeric(20, 0) NOT NULL CHECK (defaults_version = 1),
    frozen_direct_model_selection_id uuid NOT NULL,
    task_content text NOT NULL CHECK (
        task_content <> '' AND octet_length(task_content) <= 1048576
    ),
    UNIQUE (turn_id, child_session_id, admission_position),
    UNIQUE (spawning_tool_request_id, child_session_id, semantic_entry_id),
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

-- Configuration ancestry follows delegated initial tasks as well as accepted
-- input. This makes a delegated child a valid spawning parent without
-- inventing an accepted-input origin for its first turn.
CREATE OR REPLACE FUNCTION turn_origin_effective_model_configuration(
    checked_turn_id uuid,
    checked_session_id uuid
)
RETURNS TABLE (
    defaults_version numeric(20, 0),
    direct_selection_id uuid
)
LANGUAGE sql
STABLE
AS $$
    WITH RECURSIVE configuration_chain AS (
        (
            SELECT
                origin.turn_id,
                origin.source_configuration_turn_id,
                origin.defaults_version,
                COALESCE(
                    origin.frozen_direct_model_selection_id,
                    origin.frozen_alias_selected_direct_id
                ) AS direct_selection_id,
                ARRAY[origin.turn_id]::uuid[] AS visited_turn_ids
              FROM queued_input_origin AS origin
             WHERE origin.turn_id = checked_turn_id
               AND origin.session_id = checked_session_id

            UNION ALL

            SELECT
                task.turn_id,
                NULL::uuid AS source_configuration_turn_id,
                task.defaults_version,
                task.frozen_direct_model_selection_id,
                ARRAY[task.turn_id]::uuid[] AS visited_turn_ids
              FROM session_delegation_initial_task AS task
             WHERE task.turn_id = checked_turn_id
               AND task.child_session_id = checked_session_id
        )

        UNION ALL

        SELECT
            source.turn_id,
            source.source_configuration_turn_id,
            source.defaults_version,
            COALESCE(
                source.frozen_direct_model_selection_id,
                source.frozen_alias_selected_direct_id
            ),
            chain.visited_turn_ids || source.turn_id
          FROM configuration_chain AS chain
          JOIN queued_input_origin AS source
            ON source.turn_id = chain.source_configuration_turn_id
           AND source.session_id = checked_session_id
         WHERE NOT source.turn_id = ANY(chain.visited_turn_ids)
    )
    SELECT chain.defaults_version, chain.direct_selection_id
      FROM configuration_chain AS chain
     WHERE chain.defaults_version IS NOT NULL
       AND chain.direct_selection_id IS NOT NULL
     ORDER BY cardinality(chain.visited_turn_ids) DESC
     LIMIT 1
$$;

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
          JOIN session_defaults_version AS parent_defaults
            ON parent_defaults.session_id = relation.parent_session_id
           AND parent_defaults.version = frozen.defaults_version
          JOIN session_defaults_version AS child_defaults
            ON child_defaults.session_id = NEW.child_session_id
           AND child_defaults.version = NEW.defaults_version
          JOIN session_current_defaults AS child_current
            ON child_current.session_id = NEW.child_session_id
           AND child_current.current_version = NEW.defaults_version
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
           AND child_defaults.model_selection_kind =
                parent_defaults.model_selection_kind
           AND child_defaults.model_selection_reference =
                parent_defaults.model_selection_reference
           AND child_defaults.dangerous_tool_auto_approval =
                parent_defaults.dangerous_tool_auto_approval
           AND child_defaults.system_prompt_digest =
                parent_defaults.system_prompt_digest
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

-- The delegated task is the child's typed semantic turn origin. It is not an
-- accepted-input surrogate, and both sides of the relation bind the spawn.
ALTER TABLE semantic_transcript_entry
    ADD COLUMN delegated_task_spawning_tool_request_id uuid;
DO $$
DECLARE
    legacy_kind text;
    legacy_shape text;
    legacy_payload_nulls text;
BEGIN
    SELECT pg_get_expr(conbin, conrelid) INTO legacy_kind FROM pg_constraint
     WHERE conrelid = 'semantic_transcript_entry'::regclass
       AND conname = 'semantic_transcript_entry_payload_kind_closed';
    SELECT pg_get_expr(conbin, conrelid) INTO legacy_shape FROM pg_constraint
     WHERE conrelid = 'semantic_transcript_entry'::regclass
       AND conname = 'semantic_transcript_entry_payload_shape';
    SELECT string_agg(format('%I IS NULL', attname), ' AND ')
      INTO legacy_payload_nulls FROM pg_attribute
     WHERE attrelid = 'semantic_transcript_entry'::regclass
       AND attnum > 0 AND NOT attisdropped
       AND attname NOT IN (
            'source_session_id', 'semantic_entry_id', 'payload_kind',
            'delegated_task_spawning_tool_request_id'
       );
    IF legacy_kind IS NULL OR legacy_shape IS NULL
        OR legacy_payload_nulls IS NULL THEN
        RAISE EXCEPTION 'semantic transcript legacy delegated-task shape is missing';
    END IF;
    ALTER TABLE semantic_transcript_entry
        DROP CONSTRAINT semantic_transcript_entry_payload_kind_closed,
        DROP CONSTRAINT semantic_transcript_entry_payload_shape;
    EXECUTE format(
        'ALTER TABLE semantic_transcript_entry
             ADD CONSTRAINT semantic_transcript_entry_payload_kind_closed
                 CHECK (payload_kind = ''delegated_task'' OR (%s)),
             ADD CONSTRAINT semantic_transcript_entry_payload_shape CHECK (
                 (payload_kind = ''delegated_task''
                    AND delegated_task_spawning_tool_request_id IS NOT NULL
                    AND %s)
                 OR (payload_kind <> ''delegated_task''
                    AND delegated_task_spawning_tool_request_id IS NULL
                    AND (%s))
             )',
        legacy_kind, legacy_payload_nulls, legacy_shape
    );
END;
$$;
ALTER TABLE semantic_transcript_entry
    ADD CONSTRAINT semantic_transcript_entry_delegated_task_key
        UNIQUE (
            delegated_task_spawning_tool_request_id,
            source_session_id,
            semantic_entry_id
        );
ALTER TABLE session_delegation_initial_task
    ADD CONSTRAINT session_delegation_initial_task_semantic_fk
        FOREIGN KEY (
            spawning_tool_request_id, child_session_id, semantic_entry_id
        ) REFERENCES semantic_transcript_entry(
            delegated_task_spawning_tool_request_id,
            source_session_id,
            semantic_entry_id
        ) ON UPDATE RESTRICT ON DELETE NO ACTION DEFERRABLE INITIALLY DEFERRED;
ALTER TABLE semantic_transcript_entry
    ADD CONSTRAINT semantic_transcript_entry_delegated_task_fk
        FOREIGN KEY (
            delegated_task_spawning_tool_request_id,
            source_session_id,
            semantic_entry_id
        ) REFERENCES session_delegation_initial_task(
            spawning_tool_request_id, child_session_id, semantic_entry_id
        ) ON UPDATE RESTRICT ON DELETE NO ACTION DEFERRABLE INITIALLY DEFERRED;

DROP TRIGGER semantic_entry_requires_matching_turn_state
    ON semantic_transcript_entry;
CREATE CONSTRAINT TRIGGER semantic_entry_requires_matching_turn_state
AFTER INSERT ON semantic_transcript_entry DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW WHEN (NEW.payload_kind <> 'delegated_task')
EXECUTE FUNCTION require_semantic_entry_turn_state();
CREATE CONSTRAINT TRIGGER semantic_entry_update_requires_matching_turn_state
AFTER UPDATE ON semantic_transcript_entry DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW WHEN (
    OLD.payload_kind <> 'delegated_task' OR NEW.payload_kind <> 'delegated_task'
)
EXECUTE FUNCTION require_semantic_entry_turn_state();
CREATE CONSTRAINT TRIGGER semantic_entry_delete_requires_matching_turn_state
AFTER DELETE ON semantic_transcript_entry DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW WHEN (OLD.payload_kind <> 'delegated_task')
EXECUTE FUNCTION require_semantic_entry_turn_state();

-- Existing lifecycle validators predate delegated origins. Keep their full
-- attempt/frontier/terminal checks and replace only the origin lookup with this
-- closed accepted-input-or-delegated-task projection.
CREATE FUNCTION turn_lifecycle_origin_semantic_entry(
    checked_turn_id uuid,
    checked_session_id uuid,
    checked_origin_input_id uuid
)
RETURNS TABLE (semantic_entry_id uuid)
LANGUAGE sql
STABLE
AS $$
    SELECT entry.semantic_entry_id
      FROM turn_lifecycle AS lifecycle
      JOIN semantic_transcript_entry AS entry
        ON entry.source_session_id = lifecycle.session_id
      LEFT JOIN session_delegation_initial_task AS task
        ON task.turn_id = lifecycle.turn_id
       AND task.child_session_id = lifecycle.session_id
       AND task.semantic_entry_id = entry.semantic_entry_id
     WHERE lifecycle.turn_id = checked_turn_id
       AND lifecycle.session_id = checked_session_id
       AND (
            (lifecycle.origin_kind = 'accepted_input'
                AND entry.payload_kind = 'origin_accepted_input'
                AND entry.origin_accepted_input_id = checked_origin_input_id)
            OR (lifecycle.origin_kind = 'delegation'
                AND lifecycle.state_kind <> 'queued'
                AND entry.payload_kind = 'delegated_task'
                AND task.spawning_tool_request_id =
                    entry.delegated_task_spawning_tool_request_id)
       )
$$;

DO $migration$
DECLARE
    lifecycle_definition text;
    updated_definition text;
    accepted_count CONSTANT text := $old$
    SELECT count(*)
      INTO origin_entry_count
      FROM semantic_transcript_entry
     WHERE source_session_id = checked_session_id
       AND payload_kind = 'origin_accepted_input'
       AND origin_accepted_input_id = checked_origin_input_id;
$old$;
    typed_count CONSTANT text := $new$
    SELECT count(*)
      INTO origin_entry_count
      FROM turn_lifecycle_origin_semantic_entry(
            checked_turn_id,
            checked_session_id,
            checked_origin_input_id
      );
$new$;
    accepted_entry CONSTANT text := $old$
    SELECT semantic_entry_id
      INTO origin_entry_id
      FROM semantic_transcript_entry
     WHERE source_session_id = checked_session_id
       AND payload_kind = 'origin_accepted_input'
       AND origin_accepted_input_id = checked_origin_input_id;
$old$;
    typed_entry CONSTANT text := $new$
    SELECT semantic_entry_id
      INTO origin_entry_id
      FROM turn_lifecycle_origin_semantic_entry(
            checked_turn_id,
            checked_session_id,
            checked_origin_input_id
      );
$new$;
BEGIN
    SELECT pg_get_functiondef(
        'assert_turn_lifecycle_final_state_without_steering(uuid)'::regprocedure
    ) INTO lifecycle_definition;
    IF strpos(lifecycle_definition, accepted_count) = 0
        OR strpos(lifecycle_definition, accepted_entry) = 0 THEN
        RAISE EXCEPTION 'delegation could not locate lifecycle origin assertions';
    END IF;
    updated_definition := replace(
        replace(lifecycle_definition, accepted_count, typed_count),
        accepted_entry,
        typed_entry
    );
    EXECUTE updated_definition;
END;
$migration$;

DO $migration$
DECLARE
    lifecycle_definition text;
    updated_definition text;
    accepted_count CONSTANT text := $old$
    SELECT count(*)
      INTO origin_entry_count
      FROM semantic_transcript_entry
     WHERE source_session_id = checked_session
       AND payload_kind = 'origin_accepted_input'
       AND origin_accepted_input_id = checked_origin_input;
$old$;
    typed_count CONSTANT text := $new$
    SELECT count(*)
      INTO origin_entry_count
      FROM turn_lifecycle_origin_semantic_entry(
            checked_turn_id,
            checked_session,
            checked_origin_input
      );
$new$;
    accepted_entry CONSTANT text := $old$
    SELECT semantic_entry_id
      INTO origin_entry
      FROM semantic_transcript_entry
     WHERE source_session_id = checked_session
       AND payload_kind = 'origin_accepted_input'
       AND origin_accepted_input_id = checked_origin_input;
$old$;
    typed_entry CONSTANT text := $new$
    SELECT semantic_entry_id
      INTO origin_entry
      FROM turn_lifecycle_origin_semantic_entry(
            checked_turn_id,
            checked_session,
            checked_origin_input
      );
$new$;
BEGIN
    SELECT pg_get_functiondef(
        'assert_terminal_started_turn_common_final_state(uuid)'::regprocedure
    ) INTO lifecycle_definition;
    IF strpos(lifecycle_definition, accepted_count) = 0
        OR strpos(lifecycle_definition, accepted_entry) = 0 THEN
        RAISE EXCEPTION 'delegation could not locate terminal origin assertions';
    END IF;
    updated_definition := replace(
        replace(lifecycle_definition, accepted_count, typed_count),
        accepted_entry,
        typed_entry
    );
    EXECUTE updated_definition;
END;
$migration$;

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

ALTER TABLE session_delegation_wait
    ADD CONSTRAINT session_delegation_wait_attempt_key
        UNIQUE (
            awaiting_tool_request_id,
            spawning_tool_request_id,
            child_session_id
        ),
    ADD CONSTRAINT session_delegation_wait_parent_turn_key
        UNIQUE (
            awaiting_tool_request_id,
            parent_turn_id,
            parent_session_id
        );

ALTER TABLE tool_attempt
    ADD COLUMN wait_spawning_request_id uuid,
    ADD COLUMN wait_child_session_id uuid,
    DROP CONSTRAINT tool_attempt_disposition_closed;
ALTER TABLE tool_attempt
    ADD CONSTRAINT tool_attempt_disposition_closed CHECK (
        terminal_disposition_kind IS NULL OR terminal_disposition_kind IN (
            'completed', 'known_failed', 'awaiting_child', 'ambiguous'
        )
    ),
    ADD CONSTRAINT tool_attempt_child_wait_shape CHECK (
        (terminal_disposition_kind = 'awaiting_child'
            AND wait_spawning_request_id IS NOT NULL
            AND wait_child_session_id IS NOT NULL)
        OR (terminal_disposition_kind IS DISTINCT FROM 'awaiting_child'
            AND wait_spawning_request_id IS NULL
            AND wait_child_session_id IS NULL)
    ),
    ADD CONSTRAINT tool_attempt_child_wait_fk
        FOREIGN KEY (request_id, wait_spawning_request_id, wait_child_session_id)
        REFERENCES session_delegation_wait (
            awaiting_tool_request_id, spawning_tool_request_id, child_session_id
        ) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;

DO $migration$
DECLARE legacy_shape text;
BEGIN
    SELECT pg_get_expr(conbin, conrelid) INTO legacy_shape
      FROM pg_constraint
     WHERE conrelid = 'tool_attempt'::regclass
       AND conname = 'tool_attempt_state_payload_shape';
    IF legacy_shape IS NULL THEN
        RAISE EXCEPTION 'tool-attempt legacy payload shape is missing';
    END IF;
    ALTER TABLE tool_attempt DROP CONSTRAINT tool_attempt_state_payload_shape;
    EXECUTE format(
        'ALTER TABLE tool_attempt
         ADD CONSTRAINT tool_attempt_state_payload_shape CHECK (
            (%s) OR (
                state_kind = ''terminal''
                AND terminal_disposition_kind = ''awaiting_child''
                AND effect_class = ''effect_free''
                AND result_content_kind IS NULL AND result_text IS NULL
                AND error_kind IS NULL AND error_detail IS NULL
            )
         )',
        legacy_shape
    );
END;
$migration$;

-- The runner view predates the appended wait-provenance columns. Replace its
-- projection so reconstitution sees the complete current attempt row.
CREATE OR REPLACE VIEW runner_current_tool_attempt AS
SELECT attempt.*
  FROM tool_attempt AS attempt
 WHERE NOT (
        attempt.state_kind = 'terminal'
        AND EXISTS (
            SELECT 1
              FROM runner_lease_generation AS generation
              JOIN runner_current_lease_event AS current_event
                ON current_event.lease_id = generation.lease_id
               AND current_event.generation = generation.generation
              JOIN runner_lease_event AS event
                ON event.lease_id = current_event.lease_id
               AND event.generation = current_event.generation
               AND event.event_ordinal = current_event.event_ordinal
             WHERE generation.attempt_id = attempt.attempt_id
               AND generation.effect_class IN ('pure', 'idempotent')
               AND event.state_kind IN (
                    'lost_execution_possible',
                    'lost_claimed'
               )
        )
 );

ALTER TABLE turn_lifecycle
    ADD COLUMN child_wait_request_id uuid,
    DROP CONSTRAINT turn_lifecycle_active_phase_closed;
ALTER TABLE turn_lifecycle
    ADD CONSTRAINT turn_lifecycle_active_phase_closed CHECK (
        active_phase_kind IS NULL OR active_phase_kind IN (
            'running', 'awaiting_model_call_recovery', 'awaiting_tool_approval',
            'awaiting_child', 'awaiting_tool_recovery'
        )
    ),
    ADD CONSTRAINT turn_lifecycle_child_wait_shape CHECK (
        (active_phase_kind = 'awaiting_child' AND child_wait_request_id IS NOT NULL)
        OR (active_phase_kind IS DISTINCT FROM 'awaiting_child'
            AND child_wait_request_id IS NULL)
    ),
    ADD CONSTRAINT turn_lifecycle_child_wait_fk
        FOREIGN KEY (child_wait_request_id, turn_id, session_id)
        REFERENCES session_delegation_wait (
            awaiting_tool_request_id, parent_turn_id, parent_session_id
        ) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;

DO $migration$
DECLARE legacy_shape text;
BEGIN
    SELECT pg_get_expr(conbin, conrelid) INTO legacy_shape
      FROM pg_constraint
     WHERE conrelid = 'turn_lifecycle'::regclass
       AND conname = 'turn_lifecycle_state_payload_shape';
    IF legacy_shape IS NULL THEN
        RAISE EXCEPTION 'turn-lifecycle legacy payload shape is missing';
    END IF;
    ALTER TABLE turn_lifecycle
        DROP CONSTRAINT turn_lifecycle_state_payload_shape;
    EXECUTE format(
        'ALTER TABLE turn_lifecycle
         ADD CONSTRAINT turn_lifecycle_state_payload_shape CHECK (
            (%s) OR (
                state_kind = ''active''
                AND start_lineage_kind IS NOT NULL
                AND starting_frontier_id IS NOT NULL
                AND terminal_frontier_id IS NULL
                AND active_phase_kind = ''awaiting_child''
                AND current_attempt_id IS NULL
                AND terminal_disposition_kind IS NULL
                AND recovery_model_call_id IS NULL
                AND active_tool_round_call_id IS NOT NULL
                AND approval_tool_request_id IS NULL
                AND recovery_tool_attempt_id IS NULL
                AND terminal_attempt_id IS NULL
                AND terminal_model_call_id IS NULL
                AND terminal_tool_attempt_id IS NULL
            )
         )',
        legacy_shape
    );
END;
$migration$;

CREATE FUNCTION require_delegation_wait_turn_phase()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    matching_phase_count bigint;
BEGIN
    SELECT count(*) INTO matching_phase_count
      FROM turn_lifecycle
     WHERE turn_id = NEW.parent_turn_id
       AND session_id = NEW.parent_session_id
       AND state_kind = 'active'
       AND active_phase_kind = 'awaiting_child'
       AND child_wait_request_id = NEW.awaiting_tool_request_id;
    IF (NEW.wait_mode = 'foreground' AND matching_phase_count <> 1)
        OR (NEW.wait_mode = 'background' AND matching_phase_count <> 0) THEN
        RAISE EXCEPTION 'delegation wait mode contradicts its parent turn phase'
            USING ERRCODE = '23514',
                CONSTRAINT = 'session_delegation_wait_turn_phase';
    END IF;
    RETURN NULL;
END;
$$;
CREATE CONSTRAINT TRIGGER session_delegation_wait_requires_turn_phase
AFTER INSERT ON session_delegation_wait DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION require_delegation_wait_turn_phase();

CREATE FUNCTION require_turn_child_wait_mode()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.active_phase_kind = 'awaiting_child' AND NOT EXISTS (
        SELECT 1 FROM session_delegation_wait
         WHERE awaiting_tool_request_id = NEW.child_wait_request_id
           AND parent_turn_id = NEW.turn_id
           AND parent_session_id = NEW.session_id
           AND wait_mode = 'foreground'
    ) THEN
        RAISE EXCEPTION 'child-wait turn phase requires an exact foreground wait'
            USING ERRCODE = '23514',
                CONSTRAINT = 'turn_lifecycle_child_wait_mode';
    END IF;
    RETURN NULL;
END;
$$;
CREATE CONSTRAINT TRIGGER turn_lifecycle_requires_child_wait_mode
AFTER INSERT OR UPDATE ON turn_lifecycle DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION require_turn_child_wait_mode();

DO $migration$
DECLARE
    lifecycle_definition text;
    updated_definition text;
    prior_active_check CONSTANT text := $old$
        IF checked_active_phase = 'running' THEN
            IF live_attempt_count <> 1 OR exact_attempt_count <> 1 THEN
                RAISE EXCEPTION 'running turn % requires its exact live attempt', checked_turn_id
                    USING ERRCODE = '23514';
            END IF;
        ELSE
$old$;
    delegated_active_check CONSTANT text := $new$
        IF checked_active_phase = 'running' THEN
            IF live_attempt_count <> 1 OR exact_attempt_count <> 1 THEN
                RAISE EXCEPTION 'running turn % requires its exact live attempt', checked_turn_id
                    USING ERRCODE = '23514';
            END IF;
        ELSIF checked_active_phase = 'awaiting_child' THEN
            IF live_attempt_count <> 0 OR exact_attempt_count <> 0 THEN
                RAISE EXCEPTION 'child-wait turn % retains a live current attempt', checked_turn_id
                    USING ERRCODE = '23514';
            END IF;
        ELSE
$new$;
BEGIN
    SELECT pg_get_functiondef(
        'assert_turn_lifecycle_final_state_without_steering(uuid)'::regprocedure
    ) INTO lifecycle_definition;
    IF strpos(lifecycle_definition, prior_active_check) = 0 THEN
        RAISE EXCEPTION 'delegation could not locate active lifecycle assertion';
    END IF;
    updated_definition := replace(
        lifecycle_definition,
        prior_active_check,
        delegated_active_check
    );
    EXECUTE updated_definition;
END;
$migration$;

-- Preserve the pre-delegation checker for every existing phase. A foreground
-- child wait is attempt-free only after its exact authorized await attempt has
-- ended and the issuing turn attempt has yielded to the durable wait.
ALTER FUNCTION assert_tool_loop_turn_final_state(uuid)
    RENAME TO assert_tool_loop_turn_final_state_pre_delegation;

CREATE FUNCTION assert_tool_loop_turn_final_state(
    checked_turn_id uuid
)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    lifecycle turn_lifecycle%ROWTYPE;
    attempt_count bigint;
    initial_attempt_count bigint;
    linked_attempt_count bigint;
    live_attempt_count bigint;
    matching_wait_count bigint;
    round_id uuid;
BEGIN
    SELECT *
      INTO lifecycle
      FROM turn_lifecycle
     WHERE turn_id = checked_turn_id;
    IF NOT FOUND THEN
        RETURN;
    END IF;

    IF lifecycle.state_kind IS DISTINCT FROM 'active'
       OR lifecycle.active_phase_kind IS DISTINCT FROM 'awaiting_child'
    THEN
        PERFORM assert_tool_loop_turn_final_state_pre_delegation(
            checked_turn_id
        );
        RETURN;
    END IF;

    FOR round_id IN
        SELECT producing_model_call_id
          FROM tool_round
         WHERE turn_id = lifecycle.turn_id
           AND session_id = lifecycle.session_id
    LOOP
        PERFORM assert_tool_round_final_state(round_id);
    END LOOP;

    SELECT
        count(*),
        count(*) FILTER (WHERE continued_from_attempt_id IS NULL),
        count(*) FILTER (WHERE continued_from_attempt_id IS NOT NULL),
        count(*) FILTER (WHERE state_kind <> 'ended')
      INTO
        attempt_count,
        initial_attempt_count,
        linked_attempt_count,
        live_attempt_count
      FROM turn_attempt
     WHERE turn_id = lifecycle.turn_id
       AND session_id = lifecycle.session_id;

    IF lifecycle.attempt_history_present IS DISTINCT FROM (attempt_count > 0)
       OR initial_attempt_count <> 1
       OR linked_attempt_count <> attempt_count - 1
       OR live_attempt_count <> 0
       OR EXISTS (
            SELECT 1
              FROM semantic_transcript_entry
             WHERE source_session_id = lifecycle.session_id
               AND (
                    failed_turn_id = lifecycle.turn_id
                    OR completed_turn_id = lifecycle.turn_id
                    OR cancelled_turn_id = lifecycle.turn_id
               )
               AND payload_kind IN (
                    'turn_failed',
                    'turn_completed',
                    'turn_cancelled'
               )
       )
    THEN
        RAISE EXCEPTION
            'child-wait tool-loop turn lacks one ended attempt history'
            USING ERRCODE = '23514';
    END IF;

    SELECT count(*)
      INTO matching_wait_count
      FROM session_delegation_wait AS child_wait
      JOIN tool_request AS request
        ON request.request_id = child_wait.awaiting_tool_request_id
      JOIN tool_attempt AS attempt
        ON attempt.request_id = request.request_id
       AND attempt.session_id = request.session_id
       AND attempt.turn_id = request.turn_id
      JOIN turn_attempt AS issuing_attempt
        ON issuing_attempt.turn_attempt_id = attempt.issuing_turn_attempt_id
       AND issuing_attempt.turn_id = attempt.turn_id
       AND issuing_attempt.session_id = attempt.session_id
     WHERE child_wait.awaiting_tool_request_id = lifecycle.child_wait_request_id
       AND child_wait.parent_turn_id = lifecycle.turn_id
       AND child_wait.parent_session_id = lifecycle.session_id
       AND child_wait.wait_mode = 'foreground'
       AND request.producing_model_call_id = lifecycle.active_tool_round_call_id
       AND attempt.state_kind = 'terminal'
       AND attempt.terminal_disposition_kind = 'awaiting_child'
       AND attempt.effect_class = 'effect_free'
       AND attempt.wait_spawning_request_id =
           child_wait.spawning_tool_request_id
       AND attempt.wait_child_session_id = child_wait.child_session_id
       AND issuing_attempt.state_kind = 'ended'
       AND issuing_attempt.end_variant = 'without_stop'
       AND issuing_attempt.end_disposition = 'yielded_to_durable_wait';

    IF matching_wait_count <> 1 THEN
        RAISE EXCEPTION
            'child wait lacks its exact ended await attempt and provenance'
            USING ERRCODE = '23514';
    END IF;
END;
$$;

-- Scheduling writes one proof row only after the exact parent termination
-- command applied with descendant scope. Delegation outcomes consume this row
-- rather than treating an arbitrary parent command as authority. The composite
-- foreign key also closes the command/accepted-input scope correlation.
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
    root_source_kind text NOT NULL CHECK (
        root_source_kind IN ('turn_command', 'goal_command')
    ),
    root_turn_id uuid,
    root_goal_generation numeric(20, 0),
    termination_kind text NOT NULL CHECK (
        termination_kind IN ('stopped', 'cancelled')
    ),
    descendant_scope text NOT NULL CHECK (descendant_scope = 'parent_and_descendants'),
    disposition_count numeric(20, 0) NOT NULL CHECK (
        disposition_count BETWEEN 0 AND 18446744073709551615
    ),
    CONSTRAINT session_delegation_cascade_goal_generation_positive CHECK (
        root_goal_generation IS NULL
        OR root_goal_generation BETWEEN 1 AND 18446744073709551615
    ),
    CONSTRAINT session_delegation_cascade_command_source_shape CHECK (
        (root_source_kind = 'turn_command' AND root_turn_id IS NOT NULL
            AND root_goal_generation IS NULL AND termination_kind = 'cancelled')
        OR (root_source_kind = 'goal_command' AND root_turn_id IS NULL
            AND root_goal_generation IS NOT NULL AND termination_kind = 'stopped')
    ),
    FOREIGN KEY (root_command_id) REFERENCES durable_command(command_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (root_turn_id, root_session_id)
        REFERENCES turn_lifecycle(turn_id, session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED
);

CREATE TABLE session_delegation_parent_termination (
    spawning_tool_request_id uuid NOT NULL,
    root_command_id uuid NOT NULL,
    parent_session_id uuid NOT NULL,
    command_source_kind text NOT NULL CHECK (
        command_source_kind IN ('turn_command', 'goal_command')
    ),
    parent_turn_id uuid,
    parent_goal_generation numeric(20, 0),
    termination_kind text NOT NULL CHECK (termination_kind IN ('stopped', 'cancelled')),
    source_kind text NOT NULL CHECK (source_kind IN ('root', 'parent_disposition')),
    source_spawning_tool_request_id uuid,
    PRIMARY KEY (spawning_tool_request_id, root_command_id),
    CONSTRAINT session_delegation_parent_goal_generation_positive CHECK (
        parent_goal_generation IS NULL
        OR parent_goal_generation BETWEEN 1 AND 18446744073709551615
    ),
    CONSTRAINT session_delegation_parent_command_source_shape CHECK (
        (command_source_kind = 'turn_command' AND parent_turn_id IS NOT NULL
            AND parent_goal_generation IS NULL AND termination_kind = 'cancelled')
        OR (command_source_kind = 'goal_command' AND parent_turn_id IS NULL
            AND parent_goal_generation IS NOT NULL AND termination_kind = 'stopped')
    ),
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
    IF (NEW.root_source_kind = 'goal_command' AND NOT EXISTS (
            SELECT 1
              FROM goal_command AS command
              JOIN goal_event AS event
                ON event.session_id = command.session_id
               AND event.event_ordinal = command.result_event_ordinal
             WHERE command.command_id = NEW.root_command_id
               AND command.session_id = NEW.root_session_id
               AND command.operation_kind = 'stop'
               AND command.result_kind = 'applied'
               AND command.descendant_scope = NEW.descendant_scope
               AND event.event_kind = 'user_stopped'
               AND event.generation = NEW.root_goal_generation))
        OR (NEW.root_source_kind = 'turn_command' AND NOT EXISTS (
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
    IF NEW.command_source_kind <> cascade.root_source_kind
        OR (NEW.command_source_kind = 'goal_command'
            AND NEW.parent_goal_generation IS DISTINCT FROM
                cascade.root_goal_generation) THEN
        RAISE EXCEPTION 'delegation termination source contradicts its root command'
            USING ERRCODE = '23514',
                CONSTRAINT = 'session_delegation_parent_termination_chain';
    END IF;
    IF NEW.source_kind = 'root' THEN
        IF NEW.parent_session_id <> cascade.root_session_id
            OR NEW.parent_turn_id IS DISTINCT FROM cascade.root_turn_id
            OR NEW.parent_goal_generation IS DISTINCT FROM cascade.root_goal_generation
            OR NEW.termination_kind <> cascade.termination_kind
        THEN
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
           AND source_event.outcome_kind IN (
                'child_stopped', 'child_cancelled'
           )
           AND source_event.provenance_kind = CASE cascade.root_source_kind
                WHEN 'turn_command' THEN 'parent_turn_command'
                WHEN 'goal_command' THEN 'parent_goal_command'
           END
           AND source_event.provenance_command_id = NEW.root_command_id
           AND source_event.provenance_turn_id IS NOT DISTINCT FROM
                source_authority.parent_turn_id
           AND source_event.provenance_goal_generation IS NOT DISTINCT FROM
                source_authority.parent_goal_generation
           AND source_relation.child_session_id = NEW.parent_session_id
           AND (NEW.command_source_kind = 'goal_command'
                OR source_task.turn_id = NEW.parent_turn_id)
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

CREATE FUNCTION delegation_cascade_expected_frontier(
    checked_root_session uuid,
    checked_root_kind text
)
RETURNS TABLE (
    spawning_tool_request_id uuid,
    parent_session_id uuid,
    child_session_id uuid,
    effective_parent_kind text,
    source_kind text,
    source_spawning_tool_request_id uuid,
    expected_action text
)
LANGUAGE sql
STABLE
AS $$
    WITH RECURSIVE frontier AS (
        SELECT
            relation.spawning_tool_request_id,
            relation.parent_session_id,
            relation.child_session_id,
            checked_root_kind AS effective_parent_kind,
            'root'::text AS source_kind,
            NULL::uuid AS source_spawning_tool_request_id,
            CASE
                WHEN relation.policy_kind = 'background' THEN 'keep_running'
                WHEN checked_root_kind = 'stopped' THEN relation.on_parent_stopped
                ELSE relation.on_parent_cancelled
            END AS expected_action
          FROM session_delegation AS relation
         WHERE relation.parent_session_id = checked_root_session

        UNION ALL

        SELECT
            relation.spawning_tool_request_id,
            relation.parent_session_id,
            relation.child_session_id,
            CASE parent.expected_action
                WHEN 'stop' THEN 'stopped'
                WHEN 'cancel' THEN 'cancelled'
            END AS effective_parent_kind,
            'parent_disposition'::text AS source_kind,
            parent.spawning_tool_request_id AS source_spawning_tool_request_id,
            CASE
                WHEN relation.policy_kind = 'background' THEN 'keep_running'
                WHEN parent.expected_action = 'stop' THEN relation.on_parent_stopped
                ELSE relation.on_parent_cancelled
            END AS expected_action
          FROM frontier AS parent
          JOIN session_delegation AS relation
            ON relation.parent_session_id = parent.child_session_id
         WHERE parent.expected_action IN ('stop', 'cancel')
    )
    SELECT * FROM frontier
$$;

CREATE FUNCTION require_delegation_cascade_disposition_count()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    checked_command uuid := COALESCE(NEW.root_command_id, OLD.root_command_id);
    cascade session_delegation_termination_cascade%ROWTYPE;
    expected_count bigint;
    authority_count bigint;
    outcome_count bigint;
BEGIN
    SELECT * INTO cascade
      FROM session_delegation_termination_cascade
     WHERE root_command_id = checked_command;
    IF cascade.root_command_id IS NULL THEN
        RETURN NULL;
    END IF;

    -- Spawn admission locks the same parent session row. Lock the complete
    -- currently reachable set in stable order so no descendant can appear
    -- between frontier derivation and commit.
    PERFORM 1
      FROM session
     WHERE session_id IN (
        SELECT cascade.root_session_id
        UNION
        SELECT frontier.parent_session_id
          FROM delegation_cascade_expected_frontier(
                cascade.root_session_id, cascade.termination_kind
          ) AS frontier
        UNION
        SELECT frontier.child_session_id
          FROM delegation_cascade_expected_frontier(
                cascade.root_session_id, cascade.termination_kind
          ) AS frontier
     )
     ORDER BY session_id
     FOR UPDATE;
    PERFORM 1
      FROM session_delegation
     WHERE spawning_tool_request_id IN (
        SELECT frontier.spawning_tool_request_id
          FROM delegation_cascade_expected_frontier(
                cascade.root_session_id, cascade.termination_kind
          ) AS frontier
     )
     ORDER BY spawning_tool_request_id
     FOR UPDATE;

    SELECT count(*) INTO expected_count
      FROM delegation_cascade_expected_frontier(
            cascade.root_session_id, cascade.termination_kind
      );
    SELECT count(*) INTO authority_count
      FROM delegation_cascade_expected_frontier(
            cascade.root_session_id, cascade.termination_kind
      ) AS frontier
      JOIN session_delegation_parent_termination AS authority
        ON authority.root_command_id = checked_command
       AND authority.spawning_tool_request_id =
            frontier.spawning_tool_request_id
       AND authority.parent_session_id = frontier.parent_session_id
       AND authority.termination_kind = frontier.effective_parent_kind
       AND authority.source_kind = frontier.source_kind
       AND authority.source_spawning_tool_request_id IS NOT DISTINCT FROM
            frontier.source_spawning_tool_request_id;
    SELECT count(*) INTO outcome_count
      FROM delegation_cascade_expected_frontier(
            cascade.root_session_id, cascade.termination_kind
      ) AS frontier
      JOIN session_delegation_parent_termination AS authority
        ON authority.root_command_id = checked_command
       AND authority.spawning_tool_request_id =
            frontier.spawning_tool_request_id
      JOIN session_delegation_event AS outcome
        ON outcome.spawning_tool_request_id = authority.spawning_tool_request_id
       AND outcome.event_kind = 'outcome_recorded'
       AND outcome.provenance_command_id = authority.root_command_id
       AND outcome.provenance_session_id = authority.parent_session_id
       AND outcome.provenance_turn_id IS NOT DISTINCT FROM authority.parent_turn_id
       AND outcome.provenance_goal_generation IS NOT DISTINCT FROM
            authority.parent_goal_generation
       AND outcome.reason_kind = CASE authority.termination_kind
            WHEN 'stopped' THEN 'parent_stopped_parent_and_descendants'
            WHEN 'cancelled' THEN 'parent_cancelled_parent_and_descendants'
       END
       AND (
            outcome.outcome_kind = 'already_terminal'
            OR outcome.outcome_kind = CASE frontier.expected_action
                WHEN 'keep_running' THEN 'continue_running'
                WHEN 'stop' THEN 'child_stopped'
                WHEN 'cancel' THEN 'child_cancelled'
            END
       );

    IF cascade.disposition_count <> expected_count
        OR authority_count <> expected_count
        OR outcome_count <> expected_count THEN
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
            'child_cancelled', 'continue_running', 'already_terminal'
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
        provenance_kind IN (
            'tool_request', 'child_turn',
            'parent_turn_command', 'parent_goal_command'
        )
    ),
    provenance_session_id uuid NOT NULL,
    provenance_turn_id uuid,
    provenance_goal_generation numeric(20, 0),
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
            AND provenance_goal_generation IS NULL
            AND provenance_tool_request_id IS NOT NULL AND provenance_command_id IS NULL)
        OR (provenance_kind = 'child_turn' AND provenance_turn_id IS NOT NULL
            AND provenance_goal_generation IS NULL
            AND provenance_tool_request_id IS NULL AND provenance_command_id IS NULL)
        OR (provenance_kind = 'parent_turn_command' AND provenance_turn_id IS NOT NULL
            AND provenance_goal_generation IS NULL
            AND provenance_tool_request_id IS NULL AND provenance_command_id IS NOT NULL)
        OR (provenance_kind = 'parent_goal_command' AND provenance_turn_id IS NULL
            AND provenance_goal_generation IS NOT NULL
            AND provenance_tool_request_id IS NULL AND provenance_command_id IS NOT NULL)
    ),
    CONSTRAINT session_delegation_event_goal_generation_positive CHECK (
        provenance_goal_generation IS NULL
        OR provenance_goal_generation BETWEEN 1 AND 18446744073709551615
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
    WHERE event_kind = 'outcome_recorded'
      AND provenance_kind IN ('parent_turn_command', 'parent_goal_command');

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
    IF NEW.event_kind = 'outcome_recorded'
        AND NEW.outcome_kind <> 'already_terminal'
        AND latest IS NOT NULL AND EXISTS (
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
        WHEN 'outcome_recorded' THEN CASE WHEN NEW.outcome_kind IN (
                'continue_running', 'already_terminal'
            ) THEN 0
            ELSE (SELECT count(*) FROM session_child_result
                WHERE spawning_tool_request_id = NEW.spawning_tool_request_id
                  AND event_ordinal = NEW.event_ordinal
                  AND outcome_kind = NEW.outcome_kind) END
        ELSE 0 END INTO payload_count;
    IF (NEW.event_kind = 'message_delivered' AND payload_count <> 1)
        OR (NEW.event_kind = 'outcome_recorded'
            AND NEW.outcome_kind NOT IN ('continue_running', 'already_terminal')
            AND payload_count <> 1) THEN
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
            IF NEW.provenance_kind NOT IN (
                    'parent_turn_command', 'parent_goal_command'
                )
                OR NEW.provenance_session_id <> relation_parent
                OR NOT EXISTS (
                    SELECT 1 FROM session_delegation_parent_termination AS authority
                     WHERE authority.spawning_tool_request_id =
                            NEW.spawning_tool_request_id
                       AND authority.root_command_id = NEW.provenance_command_id
                       AND authority.parent_session_id = relation_parent
                       AND authority.command_source_kind = CASE NEW.provenance_kind
                            WHEN 'parent_turn_command' THEN 'turn_command'
                            WHEN 'parent_goal_command' THEN 'goal_command'
                       END
                       AND authority.parent_turn_id IS NOT DISTINCT FROM
                            NEW.provenance_turn_id
                       AND authority.parent_goal_generation IS NOT DISTINCT FROM
                            NEW.provenance_goal_generation
                       AND authority.termination_kind = CASE NEW.reason_kind
                            WHEN 'parent_stopped_parent_and_descendants' THEN 'stopped'
                            WHEN 'parent_cancelled_parent_and_descendants' THEN 'cancelled'
                       END
                ) THEN
                RAISE EXCEPTION 'parent disposition has invalid provenance'
                    USING ERRCODE = '23514', CONSTRAINT = 'session_delegation_event_semantics';
            END IF;
            IF NEW.outcome_kind = 'already_terminal' AND NOT EXISTS (
                SELECT 1 FROM session_child_result AS prior
                 WHERE prior.spawning_tool_request_id = NEW.spawning_tool_request_id
                   AND prior.event_ordinal < NEW.event_ordinal
            ) THEN
                RAISE EXCEPTION 'already-terminal disposition lacks its prior child result'
                    USING ERRCODE = '23514',
                        CONSTRAINT = 'session_delegation_event_semantics';
            ELSIF NEW.outcome_kind = 'already_terminal' THEN
                NULL;
            ELSIF relation_policy = 'background' THEN
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
            IF NEW.outcome_kind <> 'already_terminal'
                AND NEW.outcome_kind <> expected_outcome THEN
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

ALTER TABLE session_delegation_wait
    ADD CONSTRAINT session_delegation_wait_result_delivery_key
        UNIQUE (
            awaiting_tool_request_id,
            spawning_tool_request_id,
            parent_session_id
        );

-- Pending messages and background-result deliveries share one recipient-wide,
-- positive, gap-free delivery sequence. Foreground result delivery is direct
-- and therefore has no pending-delivery position.
CREATE TABLE session_pending_delivery (
    recipient_session_id uuid NOT NULL,
    delivery_sequence numeric(20, 0) NOT NULL,
    CONSTRAINT session_pending_delivery_sequence_positive CHECK (
        delivery_sequence BETWEEN 1 AND 18446744073709551615
    ),
    delivery_kind text NOT NULL CHECK (
        delivery_kind IN ('message', 'background_result')
    ),
    PRIMARY KEY (recipient_session_id, delivery_sequence),
    UNIQUE (recipient_session_id, delivery_sequence, delivery_kind),
    FOREIGN KEY (recipient_session_id) REFERENCES session(session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT
);

CREATE FUNCTION guard_session_pending_delivery_append()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE latest numeric(20, 0);
BEGIN
    PERFORM 1 FROM session
     WHERE session_id = NEW.recipient_session_id FOR UPDATE;
    SELECT max(delivery_sequence) INTO latest FROM session_pending_delivery
     WHERE recipient_session_id = NEW.recipient_session_id;
    IF (latest IS NULL AND NEW.delivery_sequence <> 1)
        OR (latest IS NOT NULL AND NEW.delivery_sequence <> latest + 1) THEN
        RAISE EXCEPTION 'session deliveries must append contiguously'
            USING ERRCODE = '23514',
                CONSTRAINT = 'session_pending_delivery_contiguous';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER session_pending_delivery_append_guard
BEFORE INSERT ON session_pending_delivery
FOR EACH ROW EXECUTE FUNCTION guard_session_pending_delivery_append();
CREATE TRIGGER session_pending_delivery_is_append_only
BEFORE UPDATE OR DELETE ON session_pending_delivery
FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE TABLE session_message_delivery (
    message_id uuid PRIMARY KEY,
    spawning_tool_request_id uuid NOT NULL,
    recipient_session_id uuid NOT NULL,
    delivery_sequence numeric(20, 0) NOT NULL,
    delivery_kind text NOT NULL CHECK (delivery_kind = 'message'),
    UNIQUE (message_id, recipient_session_id),
    FOREIGN KEY (message_id, spawning_tool_request_id)
        REFERENCES session_message(message_id, spawning_tool_request_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (recipient_session_id, delivery_sequence, delivery_kind)
        REFERENCES session_pending_delivery(
            recipient_session_id, delivery_sequence, delivery_kind
        ) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED
);

CREATE FUNCTION require_session_message_delivery_recipient()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM session_message AS message
          JOIN session_delegation AS relation
            ON relation.spawning_tool_request_id = message.spawning_tool_request_id
         WHERE message.message_id = NEW.message_id
           AND message.spawning_tool_request_id = NEW.spawning_tool_request_id
           AND NEW.recipient_session_id = CASE message.direction
                WHEN 'parent_to_child' THEN relation.child_session_id
                WHEN 'child_to_parent' THEN relation.parent_session_id
           END
    ) THEN
        RAISE EXCEPTION 'message delivery names the wrong recipient'
            USING ERRCODE = '23514',
                CONSTRAINT = 'session_message_delivery_recipient';
    END IF;
    RETURN NULL;
END;
$$;
CREATE CONSTRAINT TRIGGER session_message_delivery_recipient
AFTER INSERT ON session_message_delivery DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION require_session_message_delivery_recipient();

CREATE TABLE session_child_result_delivery (
    awaiting_tool_request_id uuid PRIMARY KEY,
    spawning_tool_request_id uuid NOT NULL,
    parent_session_id uuid NOT NULL,
    delivery_sequence numeric(20, 0),
    delivery_kind text CHECK (delivery_kind = 'background_result'),
    UNIQUE (
        awaiting_tool_request_id,
        spawning_tool_request_id,
        parent_session_id
    ),
    CONSTRAINT session_child_result_delivery_sequence_shape CHECK (
        (delivery_sequence IS NULL AND delivery_kind IS NULL)
        OR (delivery_sequence IS NOT NULL AND delivery_kind = 'background_result')
    ),
    FOREIGN KEY (
        awaiting_tool_request_id, spawning_tool_request_id, parent_session_id
    ) REFERENCES session_delegation_wait(
        awaiting_tool_request_id, spawning_tool_request_id, parent_session_id
    ) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (spawning_tool_request_id)
        REFERENCES session_child_result(spawning_tool_request_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (parent_session_id, delivery_sequence, delivery_kind)
        REFERENCES session_pending_delivery(
            recipient_session_id, delivery_sequence, delivery_kind
        ) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED
);

CREATE FUNCTION require_session_child_result_delivery_mode()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM session_delegation_wait AS wait
         WHERE wait.awaiting_tool_request_id = NEW.awaiting_tool_request_id
           AND wait.spawning_tool_request_id = NEW.spawning_tool_request_id
           AND wait.parent_session_id = NEW.parent_session_id
           AND (wait.wait_mode = 'foreground') = (NEW.delivery_sequence IS NULL)
    ) THEN
        RAISE EXCEPTION 'result delivery position contradicts its wait mode'
            USING ERRCODE = '23514',
                CONSTRAINT = 'session_child_result_delivery_mode';
    END IF;
    RETURN NULL;
END;
$$;
CREATE CONSTRAINT TRIGGER session_child_result_delivery_mode
AFTER INSERT ON session_child_result_delivery DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION require_session_child_result_delivery_mode();

CREATE FUNCTION require_session_child_result_wait_deliveries()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    checked_request uuid := COALESCE(
        NEW.spawning_tool_request_id,
        OLD.spawning_tool_request_id
    );
    wait_count bigint;
    delivery_count bigint;
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM session_child_result
         WHERE spawning_tool_request_id = checked_request
    ) THEN
        RETURN NULL;
    END IF;
    SELECT count(*) INTO wait_count
      FROM session_delegation_wait
     WHERE spawning_tool_request_id = checked_request;
    SELECT count(*) INTO delivery_count
      FROM session_child_result_delivery
     WHERE spawning_tool_request_id = checked_request;
    IF delivery_count <> wait_count THEN
        RAISE EXCEPTION 'child result requires one delivery for every registered wait'
            USING ERRCODE = '23503',
                CONSTRAINT = 'session_child_result_wait_deliveries';
    END IF;
    RETURN NULL;
END;
$$;
CREATE CONSTRAINT TRIGGER session_delegation_wait_zz_requires_result_delivery
AFTER INSERT ON session_delegation_wait DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION require_session_child_result_wait_deliveries();
CREATE CONSTRAINT TRIGGER session_child_result_zz_requires_wait_deliveries
AFTER INSERT ON session_child_result DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION require_session_child_result_wait_deliveries();
CREATE CONSTRAINT TRIGGER child_result_delivery_zz_closes_waits
AFTER INSERT OR UPDATE OR DELETE ON session_child_result_delivery
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION require_session_child_result_wait_deliveries();

CREATE FUNCTION require_session_pending_delivery_satellite()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE satellite_count bigint;
BEGIN
    SELECT CASE NEW.delivery_kind
        WHEN 'message' THEN (SELECT count(*) FROM session_message_delivery
            WHERE recipient_session_id = NEW.recipient_session_id
              AND delivery_sequence = NEW.delivery_sequence)
        WHEN 'background_result' THEN (SELECT count(*) FROM session_child_result_delivery
            WHERE parent_session_id = NEW.recipient_session_id
              AND delivery_sequence = NEW.delivery_sequence)
    END INTO satellite_count;
    IF satellite_count <> 1 THEN
        RAISE EXCEPTION 'pending delivery requires exactly one typed satellite'
            USING ERRCODE = '23503',
                CONSTRAINT = 'session_pending_delivery_satellite';
    END IF;
    RETURN NULL;
END;
$$;
CREATE CONSTRAINT TRIGGER session_pending_delivery_requires_satellite
AFTER INSERT ON session_pending_delivery DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION require_session_pending_delivery_satellite();

CREATE FUNCTION require_session_message_delivery()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF (SELECT count(*) FROM session_message_delivery
         WHERE message_id = NEW.message_id
           AND spawning_tool_request_id = NEW.spawning_tool_request_id) <> 1 THEN
        RAISE EXCEPTION 'session message requires exactly one pending delivery'
            USING ERRCODE = '23503',
                CONSTRAINT = 'session_message_delivery_required';
    END IF;
    RETURN NULL;
END;
$$;
CREATE CONSTRAINT TRIGGER session_message_requires_delivery
AFTER INSERT ON session_message DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION require_session_message_delivery();

CREATE TRIGGER session_message_delivery_is_append_only
BEFORE UPDATE OR DELETE ON session_message_delivery
FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();
CREATE TRIGGER session_child_result_delivery_is_append_only
BEFORE UPDATE OR DELETE ON session_child_result_delivery
FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

-- One immutable child result may satisfy multiple waits, but each semantic
-- delivery retains the exact await request that receives it.
ALTER TABLE semantic_transcript_entry
    ADD COLUMN delegation_message_id uuid,
    ADD COLUMN delegation_result_awaiting_tool_request_id uuid,
    ADD COLUMN delegation_result_spawning_tool_request_id uuid;

DO $$
DECLARE
    legacy_kind text;
    legacy_shape text;
    legacy_payload_nulls text;
BEGIN
    SELECT pg_get_expr(constraint_record.conbin, constraint_record.conrelid)
      INTO legacy_kind
      FROM pg_constraint AS constraint_record
     WHERE constraint_record.conrelid = 'semantic_transcript_entry'::regclass
       AND constraint_record.conname = 'semantic_transcript_entry_payload_kind_closed';
    SELECT pg_get_expr(constraint_record.conbin, constraint_record.conrelid)
      INTO legacy_shape
      FROM pg_constraint AS constraint_record
     WHERE constraint_record.conrelid = 'semantic_transcript_entry'::regclass
       AND constraint_record.conname = 'semantic_transcript_entry_payload_shape';
    SELECT string_agg(format('%I IS NULL', attribute.attname), ' AND ')
      INTO legacy_payload_nulls
      FROM pg_attribute AS attribute
     WHERE attribute.attrelid = 'semantic_transcript_entry'::regclass
       AND attribute.attnum > 0
       AND NOT attribute.attisdropped
       AND attribute.attname NOT IN (
            'source_session_id', 'semantic_entry_id', 'payload_kind',
            'delegation_message_id',
            'delegation_result_awaiting_tool_request_id',
            'delegation_result_spawning_tool_request_id'
       );
    IF legacy_kind IS NULL OR legacy_shape IS NULL
        OR legacy_payload_nulls IS NULL THEN
        RAISE EXCEPTION 'semantic transcript legacy delegation shape is missing';
    END IF;

    ALTER TABLE semantic_transcript_entry
        DROP CONSTRAINT semantic_transcript_entry_payload_kind_closed,
        DROP CONSTRAINT semantic_transcript_entry_payload_shape;
    EXECUTE format(
        'ALTER TABLE semantic_transcript_entry
             ADD CONSTRAINT semantic_transcript_entry_payload_kind_closed
                 CHECK (payload_kind IN (
                    ''delegation_message'', ''delegation_result''
                 ) OR (%s)),
             ADD CONSTRAINT semantic_transcript_entry_payload_shape CHECK (
                 (payload_kind = ''delegation_message''
                    AND delegation_message_id IS NOT NULL
                    AND delegation_result_awaiting_tool_request_id IS NULL
                    AND delegation_result_spawning_tool_request_id IS NULL
                    AND %s)
                 OR (payload_kind = ''delegation_result''
                    AND delegation_message_id IS NULL
                    AND delegation_result_awaiting_tool_request_id IS NOT NULL
                    AND delegation_result_spawning_tool_request_id IS NOT NULL
                    AND %s)
                 OR (payload_kind NOT IN (
                        ''delegation_message'', ''delegation_result''
                    )
                    AND delegation_message_id IS NULL
                    AND delegation_result_awaiting_tool_request_id IS NULL
                    AND delegation_result_spawning_tool_request_id IS NULL
                    AND (%s))
             )',
        legacy_kind,
        legacy_payload_nulls,
        legacy_payload_nulls,
        legacy_shape
    );
END;
$$;

ALTER TABLE semantic_transcript_entry
    ADD CONSTRAINT semantic_transcript_entry_delegation_message_once
        UNIQUE (delegation_message_id),
    ADD CONSTRAINT semantic_transcript_entry_delegation_message_delivery_fk
        FOREIGN KEY (delegation_message_id, source_session_id)
        REFERENCES session_message_delivery(message_id, recipient_session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    ADD CONSTRAINT semantic_transcript_entry_delegation_result_await_once
        UNIQUE (delegation_result_awaiting_tool_request_id),
    ADD CONSTRAINT semantic_transcript_entry_delegation_result_delivery_fk
        FOREIGN KEY (
            delegation_result_awaiting_tool_request_id,
            delegation_result_spawning_tool_request_id,
            source_session_id
        ) REFERENCES session_child_result_delivery(
            awaiting_tool_request_id,
            spawning_tool_request_id,
            parent_session_id
        ) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;

-- Delegation messages and results are authorized by their exact delivery
-- foreign keys, not by a fabricated producing turn in the recipient session.
DROP TRIGGER semantic_entry_requires_matching_turn_state
    ON semantic_transcript_entry;
DROP TRIGGER semantic_entry_update_requires_matching_turn_state
    ON semantic_transcript_entry;
DROP TRIGGER semantic_entry_delete_requires_matching_turn_state
    ON semantic_transcript_entry;
CREATE CONSTRAINT TRIGGER semantic_entry_requires_matching_turn_state
AFTER INSERT ON semantic_transcript_entry
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW WHEN (
    NEW.payload_kind NOT IN (
        'delegated_task', 'delegation_message', 'delegation_result'
    )
)
EXECUTE FUNCTION require_semantic_entry_turn_state();
CREATE CONSTRAINT TRIGGER semantic_entry_update_requires_matching_turn_state
AFTER UPDATE ON semantic_transcript_entry
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW WHEN (
    OLD.payload_kind NOT IN (
        'delegated_task', 'delegation_message', 'delegation_result'
    )
    OR NEW.payload_kind NOT IN (
        'delegated_task', 'delegation_message', 'delegation_result'
    )
)
EXECUTE FUNCTION require_semantic_entry_turn_state();
CREATE CONSTRAINT TRIGGER semantic_entry_delete_requires_matching_turn_state
AFTER DELETE ON semantic_transcript_entry
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW WHEN (
    OLD.payload_kind NOT IN (
        'delegated_task', 'delegation_message', 'delegation_result'
    )
)
EXECUTE FUNCTION require_semantic_entry_turn_state();

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
            'child_cancelled', 'continue_running', 'already_terminal'
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
        provenance_kind IS NULL OR provenance_kind IN (
            'child_turn', 'parent_turn_command', 'parent_goal_command'
        )
    ),
    provenance_session_id uuid,
    provenance_turn_id uuid,
    provenance_goal_generation numeric(20, 0) CHECK (
        provenance_goal_generation IS NULL
        OR provenance_goal_generation BETWEEN 1 AND 18446744073709551615
    ),
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
            AND provenance_turn_id IS NULL
            AND provenance_goal_generation IS NULL
            AND provenance_command_id IS NULL)
        OR (provenance_kind = 'child_turn'
            AND provenance_session_id IS NOT NULL
            AND provenance_turn_id IS NOT NULL
            AND provenance_goal_generation IS NULL
            AND provenance_command_id IS NULL)
        OR (provenance_kind = 'parent_turn_command'
            AND provenance_session_id IS NOT NULL
            AND provenance_turn_id IS NOT NULL
            AND provenance_goal_generation IS NULL
            AND provenance_command_id IS NOT NULL)
        OR (provenance_kind = 'parent_goal_command'
            AND provenance_session_id IS NOT NULL
            AND provenance_turn_id IS NULL
            AND provenance_goal_generation IS NOT NULL
            AND provenance_command_id IS NOT NULL)
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
        ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
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
    ON delegation_update_outbox_event(spawning_tool_request_id)
    WHERE update_kind = 'child_spawned';
CREATE UNIQUE INDEX delegation_child_waiting_update_once
    ON delegation_update_outbox_event(awaiting_tool_request_id)
    WHERE update_kind = 'child_waiting';
CREATE UNIQUE INDEX delegation_lifecycle_update_once
    ON delegation_update_outbox_event(
        spawning_tool_request_id, delegation_event_ordinal
    ) WHERE update_kind = 'child_lifecycle_disposition';
CREATE UNIQUE INDEX delegation_child_result_update_once
    ON delegation_update_outbox_event(result_spawning_request_id)
    WHERE update_kind = 'child_result';
CREATE UNIQUE INDEX delegation_session_message_update_once
    ON delegation_update_outbox_event(message_id)
    WHERE update_kind = 'session_message';
CREATE FUNCTION require_delegation_update_subject()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM session_delegation AS relation
         WHERE relation.spawning_tool_request_id = NEW.spawning_tool_request_id
           AND CASE NEW.update_kind
                WHEN 'child_spawned' THEN
                    NEW.session_id = relation.parent_session_id
                    AND NEW.child_session_id = relation.child_session_id
                    AND NEW.policy_kind = relation.policy_kind
                    AND NEW.on_parent_stopped IS NOT DISTINCT FROM relation.on_parent_stopped
                    AND NEW.on_parent_cancelled IS NOT DISTINCT FROM relation.on_parent_cancelled
                WHEN 'child_waiting' THEN EXISTS (
                    SELECT 1 FROM session_delegation_wait AS wait
                     WHERE wait.awaiting_tool_request_id = NEW.awaiting_tool_request_id
                       AND wait.spawning_tool_request_id = NEW.spawning_tool_request_id
                       AND wait.child_session_id = NEW.child_session_id
                       AND wait.wait_mode = NEW.wait_mode
                       AND NEW.session_id = relation.parent_session_id
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
                       AND event.provenance_turn_id IS NOT DISTINCT FROM
                            NEW.provenance_turn_id
                       AND event.provenance_goal_generation IS NOT DISTINCT FROM
                            NEW.provenance_goal_generation
                       AND event.provenance_command_id IS NOT DISTINCT FROM
                            NEW.provenance_command_id
                       AND relation.child_session_id = NEW.child_session_id
                       AND NEW.session_id = relation.parent_session_id
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
                       AND event.provenance_turn_id IS NOT DISTINCT FROM
                            NEW.provenance_turn_id
                       AND event.provenance_goal_generation IS NOT DISTINCT FROM
                            NEW.provenance_goal_generation
                       AND event.provenance_command_id IS NOT DISTINCT FROM
                            NEW.provenance_command_id
                       AND relation.child_session_id = NEW.child_session_id
                       AND NEW.session_id = relation.parent_session_id
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
                       AND NEW.session_id = NEW.recipient_session_id
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

CREATE FUNCTION require_delegation_spawn_update()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF (SELECT count(*) FROM delegation_update_outbox_event
         WHERE update_kind = 'child_spawned'
           AND spawning_tool_request_id = NEW.spawning_tool_request_id
           AND session_id = NEW.parent_session_id) <> 1 THEN
        RAISE EXCEPTION 'delegation relation requires exactly one child-spawned update'
            USING ERRCODE = '23503',
                CONSTRAINT = 'delegation_child_spawned_update_required';
    END IF;
    RETURN NULL;
END;
$$;
CREATE CONSTRAINT TRIGGER session_delegation_zz_requires_spawn_update
AFTER INSERT ON session_delegation DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION require_delegation_spawn_update();

CREATE FUNCTION require_delegation_wait_update()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF (SELECT count(*) FROM delegation_update_outbox_event
         WHERE update_kind = 'child_waiting'
           AND awaiting_tool_request_id = NEW.awaiting_tool_request_id
           AND session_id = NEW.parent_session_id) <> 1 THEN
        RAISE EXCEPTION 'delegation wait requires exactly one child-waiting update'
            USING ERRCODE = '23503',
                CONSTRAINT = 'delegation_child_waiting_update_required';
    END IF;
    RETURN NULL;
END;
$$;
CREATE CONSTRAINT TRIGGER session_delegation_wait_zz_requires_update
AFTER INSERT ON session_delegation_wait DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION require_delegation_wait_update();

CREATE FUNCTION require_delegation_lifecycle_update()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.event_kind = 'outcome_recorded' AND (
        SELECT count(*) FROM delegation_update_outbox_event AS emitted
          JOIN session_delegation AS relation
            ON relation.spawning_tool_request_id = NEW.spawning_tool_request_id
         WHERE emitted.update_kind = 'child_lifecycle_disposition'
           AND emitted.spawning_tool_request_id = NEW.spawning_tool_request_id
           AND emitted.delegation_event_ordinal = NEW.event_ordinal
           AND emitted.session_id = relation.parent_session_id
    ) <> 1 THEN
        RAISE EXCEPTION 'delegation outcome requires exactly one lifecycle update'
            USING ERRCODE = '23503',
                CONSTRAINT = 'delegation_lifecycle_update_required';
    END IF;
    RETURN NULL;
END;
$$;
CREATE CONSTRAINT TRIGGER session_delegation_event_zz_requires_lifecycle_update
AFTER INSERT ON session_delegation_event DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION require_delegation_lifecycle_update();

CREATE FUNCTION require_delegation_message_update()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF (SELECT count(*) FROM delegation_update_outbox_event AS emitted
          JOIN session_delegation AS relation
            ON relation.spawning_tool_request_id = NEW.spawning_tool_request_id
         WHERE emitted.update_kind = 'session_message'
           AND emitted.message_id = NEW.message_id
           AND emitted.session_id = CASE NEW.direction
                WHEN 'parent_to_child' THEN relation.child_session_id
                WHEN 'child_to_parent' THEN relation.parent_session_id
           END) <> 1 THEN
        RAISE EXCEPTION 'delegation message requires exactly one message update'
            USING ERRCODE = '23503',
                CONSTRAINT = 'delegation_session_message_update_required';
    END IF;
    RETURN NULL;
END;
$$;
CREATE CONSTRAINT TRIGGER session_message_zz_requires_update
AFTER INSERT ON session_message DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION require_delegation_message_update();

CREATE FUNCTION require_delegation_result_update()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF (SELECT count(*) FROM delegation_update_outbox_event AS emitted
          JOIN session_delegation AS relation
            ON relation.spawning_tool_request_id = NEW.spawning_tool_request_id
         WHERE emitted.update_kind = 'child_result'
           AND emitted.result_spawning_request_id = NEW.spawning_tool_request_id
           AND emitted.session_id = relation.parent_session_id) <> 1 THEN
        RAISE EXCEPTION 'delegation result requires exactly one child-result update'
            USING ERRCODE = '23503',
                CONSTRAINT = 'delegation_child_result_update_required';
    END IF;
    RETURN NULL;
END;
$$;
CREATE CONSTRAINT TRIGGER session_child_result_zz_requires_update
AFTER INSERT ON session_child_result DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION require_delegation_result_update();
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

CREATE FUNCTION require_delegation_message_wake()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF (SELECT count(*) FROM delegation_wake_outbox_event AS wake
          JOIN session_delegation AS relation
            ON relation.spawning_tool_request_id = NEW.spawning_tool_request_id
         WHERE wake.subject_kind = 'message'
           AND wake.message_id = NEW.message_id
           AND wake.session_id = CASE NEW.direction
                WHEN 'parent_to_child' THEN relation.child_session_id
                WHEN 'child_to_parent' THEN relation.parent_session_id
           END) <> 1 THEN
        RAISE EXCEPTION 'delegation message requires exactly one recipient wake'
            USING ERRCODE = '23503',
                CONSTRAINT = 'delegation_message_wake_required';
    END IF;
    RETURN NULL;
END;
$$;
CREATE CONSTRAINT TRIGGER session_message_zz_requires_wake
AFTER INSERT ON session_message DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION require_delegation_message_wake();

CREATE FUNCTION require_delegation_result_wake()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF (SELECT count(*) FROM delegation_wake_outbox_event AS wake
          JOIN session_delegation AS relation
            ON relation.spawning_tool_request_id = NEW.spawning_tool_request_id
         WHERE wake.subject_kind = 'result'
           AND wake.result_spawning_request_id = NEW.spawning_tool_request_id
           AND wake.session_id = relation.parent_session_id) <> 1 THEN
        RAISE EXCEPTION 'delegation result requires exactly one parent wake'
            USING ERRCODE = '23503',
                CONSTRAINT = 'delegation_result_wake_required';
    END IF;
    RETURN NULL;
END;
$$;
CREATE CONSTRAINT TRIGGER session_child_result_zz_requires_wake
AFTER INSERT ON session_child_result DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION require_delegation_result_wake();
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
CREATE TRIGGER session_pending_delivery_cannot_be_truncated
BEFORE TRUNCATE ON session_pending_delivery
FOR EACH STATEMENT EXECUTE FUNCTION reject_session_delegation_table_truncate();
CREATE TRIGGER session_message_delivery_cannot_be_truncated
BEFORE TRUNCATE ON session_message_delivery
FOR EACH STATEMENT EXECUTE FUNCTION reject_session_delegation_table_truncate();
CREATE TRIGGER session_child_result_delivery_cannot_be_truncated
BEFORE TRUNCATE ON session_child_result_delivery
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
