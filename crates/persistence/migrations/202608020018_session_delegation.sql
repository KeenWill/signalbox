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
     FOR NO KEY UPDATE;
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

CREATE UNIQUE INDEX turn_lifecycle_one_queued_delegation_origin_per_session
    ON turn_lifecycle(session_id)
    WHERE state_kind = 'queued' AND origin_kind = 'delegation';

CREATE TABLE session_delegation_initial_task (
    spawning_tool_request_id uuid PRIMARY KEY,
    child_session_id uuid NOT NULL UNIQUE,
    turn_id uuid NOT NULL UNIQUE,
    semantic_entry_id uuid NOT NULL UNIQUE,
    admission_position numeric(20, 0) NOT NULL CHECK (admission_position = 1),
    defaults_version numeric(20, 0) NOT NULL CHECK (defaults_version = 1),
    requested_model_kind text NOT NULL,
    requested_direct_model_selection_id uuid,
    requested_model_alias_id uuid,
    frozen_model_kind text NOT NULL,
    frozen_direct_model_selection_id uuid,
    frozen_model_alias_id uuid,
    frozen_alias_selected_direct_id uuid,
    task_content text NOT NULL CHECK (
        task_content <> '' AND octet_length(task_content) <= 1048576
    ),
    CONSTRAINT session_delegation_initial_task_requested_model_shape CHECK (
        (requested_model_kind = 'direct'
            AND requested_direct_model_selection_id IS NOT NULL
            AND requested_model_alias_id IS NULL)
        OR (requested_model_kind = 'alias'
            AND requested_direct_model_selection_id IS NULL
            AND requested_model_alias_id IS NOT NULL)
    ),
    CONSTRAINT session_delegation_initial_task_frozen_model_shape CHECK (
        (frozen_model_kind = 'direct'
            AND frozen_direct_model_selection_id IS NOT NULL
            AND frozen_model_alias_id IS NULL
            AND frozen_alias_selected_direct_id IS NULL)
        OR (frozen_model_kind = 'frozen_alias'
            AND frozen_direct_model_selection_id IS NULL
            AND frozen_model_alias_id IS NOT NULL
            AND frozen_alias_selected_direct_id IS NOT NULL)
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
                COALESCE(
                    task.frozen_direct_model_selection_id,
                    task.frozen_alias_selected_direct_id
                ),
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

CREATE FUNCTION turn_origin_exact_model_configuration(
    checked_turn_id uuid,
    checked_session_id uuid
)
RETURNS TABLE (
    defaults_version numeric(20, 0),
    requested_model_kind text,
    requested_direct_model_selection_id uuid,
    requested_model_alias_id uuid,
    frozen_model_kind text,
    frozen_direct_model_selection_id uuid,
    frozen_model_alias_id uuid,
    frozen_alias_selected_direct_id uuid
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
                origin.requested_model_kind,
                origin.requested_direct_model_selection_id,
                origin.requested_model_alias_id,
                origin.frozen_model_kind,
                origin.frozen_direct_model_selection_id,
                origin.frozen_model_alias_id,
                origin.frozen_alias_selected_direct_id,
                ARRAY[origin.turn_id]::uuid[] AS visited_turn_ids
              FROM queued_input_origin AS origin
             WHERE origin.turn_id = checked_turn_id
               AND origin.session_id = checked_session_id

            UNION ALL

            SELECT
                task.turn_id,
                NULL::uuid,
                task.defaults_version,
                task.requested_model_kind,
                task.requested_direct_model_selection_id,
                task.requested_model_alias_id,
                task.frozen_model_kind,
                task.frozen_direct_model_selection_id,
                task.frozen_model_alias_id,
                task.frozen_alias_selected_direct_id,
                ARRAY[task.turn_id]::uuid[]
              FROM session_delegation_initial_task AS task
             WHERE task.turn_id = checked_turn_id
               AND task.child_session_id = checked_session_id
        )

        UNION ALL

        SELECT
            source.turn_id,
            source.source_configuration_turn_id,
            source.defaults_version,
            source.requested_model_kind,
            source.requested_direct_model_selection_id,
            source.requested_model_alias_id,
            source.frozen_model_kind,
            source.frozen_direct_model_selection_id,
            source.frozen_model_alias_id,
            source.frozen_alias_selected_direct_id,
            chain.visited_turn_ids || source.turn_id
          FROM configuration_chain AS chain
          JOIN queued_input_origin AS source
            ON source.turn_id = chain.source_configuration_turn_id
           AND source.session_id = checked_session_id
         WHERE NOT source.turn_id = ANY(chain.visited_turn_ids)
    )
    SELECT
        chain.defaults_version,
        chain.requested_model_kind,
        chain.requested_direct_model_selection_id,
        chain.requested_model_alias_id,
        chain.frozen_model_kind,
        chain.frozen_direct_model_selection_id,
        chain.frozen_model_alias_id,
        chain.frozen_alias_selected_direct_id
      FROM configuration_chain AS chain
     WHERE chain.defaults_version IS NOT NULL
       AND chain.requested_model_kind IS NOT NULL
       AND chain.frozen_model_kind IS NOT NULL
     ORDER BY cardinality(chain.visited_turn_ids) DESC
     LIMIT 1
$$;

-- The model-call validator predates delegation-origin turns. Preserve its
-- complete attempt, frontier, state, and target checks while replacing only
-- the accepted-input-only configuration lookup with the closed typed-origin
-- projection above.
DO $migration$
DECLARE
    definition text;
    updated_definition text;
    accepted_origin_lookup CONSTANT text := $old$
    WITH RECURSIVE configuration_origin AS (
        SELECT stored.*
          FROM queued_input_origin AS stored
         WHERE stored.turn_id = checked_turn_id
           AND stored.session_id = checked_session_id
        UNION
        SELECT source.*
          FROM configuration_origin AS current
          JOIN queued_input_origin AS source
            ON source.turn_id = current.source_configuration_turn_id
           AND source.session_id = current.session_id
    )
    SELECT
        origin.frozen_model_kind,
        origin.frozen_direct_model_selection_id,
        origin.frozen_model_alias_id,
        origin.frozen_alias_selected_direct_id,
        lifecycle.pinned_provider_model_identity_id,
        lifecycle.state_kind,
        lifecycle.active_phase_kind,
        lifecycle.current_attempt_id,
        lifecycle.recovery_model_call_id,
        lifecycle.terminal_attempt_id,
        lifecycle.terminal_model_call_id,
        lifecycle.terminal_disposition_kind,
        lifecycle.starting_frontier_id
      INTO
        origin_frozen_kind,
        origin_direct_id,
        origin_alias_id,
        origin_alias_selected_id,
        pinned_target_id,
        turn_state,
        active_phase,
        current_attempt,
        recovery_call,
        terminal_attempt,
        terminal_call,
        terminal_disposition,
        starting_frontier
      FROM turn_lifecycle AS lifecycle
      JOIN configuration_origin AS origin
        ON origin.session_id = lifecycle.session_id
       AND origin.source_configuration_turn_id IS NULL
     WHERE lifecycle.turn_id = checked_turn_id
       AND lifecycle.session_id = checked_session_id;
$old$;
    typed_origin_lookup CONSTANT text := $new$
    SELECT
        origin.frozen_model_kind,
        origin.frozen_direct_model_selection_id,
        origin.frozen_model_alias_id,
        origin.frozen_alias_selected_direct_id,
        lifecycle.pinned_provider_model_identity_id,
        lifecycle.state_kind,
        lifecycle.active_phase_kind,
        lifecycle.current_attempt_id,
        lifecycle.recovery_model_call_id,
        lifecycle.terminal_attempt_id,
        lifecycle.terminal_model_call_id,
        lifecycle.terminal_disposition_kind,
        lifecycle.starting_frontier_id
      INTO
        origin_frozen_kind,
        origin_direct_id,
        origin_alias_id,
        origin_alias_selected_id,
        pinned_target_id,
        turn_state,
        active_phase,
        current_attempt,
        recovery_call,
        terminal_attempt,
        terminal_call,
        terminal_disposition,
        starting_frontier
      FROM turn_lifecycle AS lifecycle
      JOIN LATERAL turn_origin_exact_model_configuration(
            lifecycle.turn_id,
            lifecycle.session_id
      ) AS origin ON TRUE
     WHERE lifecycle.turn_id = checked_turn_id
       AND lifecycle.session_id = checked_session_id;
$new$;
BEGIN
    SELECT pg_get_functiondef(
        'assert_model_call_final_state_without_stop(uuid)'::regprocedure
    ) INTO definition;
    updated_definition := replace(
        definition,
        accepted_origin_lookup,
        typed_origin_lookup
    );
    IF updated_definition = definition THEN
        RAISE EXCEPTION 'delegation could not update model-call origin lookup';
    END IF;
    EXECUTE updated_definition;
END;
$migration$;

-- Model-identity boundary validation must recognize the same closed origin
-- families as configuration resolution. A delegated initial turn has no
-- accepted-input origin row, so resolving it through queued_input_origin alone
-- would make the otherwise-valid turn impossible to activate.
CREATE OR REPLACE FUNCTION turn_start_model_identity_boundary_is_valid(
    checked_turn_id uuid,
    checked_frontier_id uuid
)
RETURNS boolean
LANGUAGE plpgsql
STABLE
AS $$
DECLARE
    checked_session uuid;
    checked_defaults_version numeric(20, 0);
    checked_selection uuid;
    boundary_required boolean;
    predecessor_turn uuid;
    predecessor_selection uuid;
    starting_member_count numeric(20, 0);
    boundary_entry_count bigint;
    boundary_member_count bigint;
    boundary_member_position numeric(20, 0);
BEGIN
    SELECT lifecycle.session_id, lifecycle.model_identity_boundary_required
      INTO checked_session, boundary_required
      FROM turn_lifecycle AS lifecycle
     WHERE lifecycle.turn_id = checked_turn_id
       AND (
            (
                lifecycle.origin_kind = 'accepted_input'
                AND EXISTS (
                    SELECT 1
                      FROM queued_input_origin AS origin
                     WHERE origin.turn_id = lifecycle.turn_id
                       AND origin.session_id = lifecycle.session_id
                )
            )
            OR (
                lifecycle.origin_kind = 'delegation'
                AND (
                    EXISTS (
                        SELECT 1
                          FROM session_delegation_initial_task AS task
                         WHERE task.turn_id = lifecycle.turn_id
                           AND task.child_session_id = lifecycle.session_id
                    )
                    OR EXISTS (
                        SELECT 1
                          FROM session_delegation_wake_turn_origin AS wake
                         WHERE wake.turn_id = lifecycle.turn_id
                           AND wake.recipient_session_id = lifecycle.session_id
                    )
                )
            )
       );

    IF NOT FOUND THEN
        RETURN false;
    END IF;

    SELECT
        effective.defaults_version,
        effective.direct_selection_id
      INTO checked_defaults_version, checked_selection
      FROM turn_origin_effective_model_configuration(
               checked_turn_id,
               checked_session
           ) AS effective;

    IF NOT FOUND THEN
        RETURN false;
    END IF;

    predecessor_turn := accepted_input_turn_queue_predecessor(
        checked_session,
        checked_turn_id
    );
    IF predecessor_turn IS NOT NULL THEN
        SELECT effective.direct_selection_id
          INTO predecessor_selection
          FROM turn_origin_effective_model_configuration(
                   predecessor_turn,
                   checked_session
               ) AS effective;
        IF NOT FOUND THEN
            RETURN false;
        END IF;
    END IF;

    SELECT count(*)
      INTO boundary_entry_count
      FROM semantic_transcript_entry AS entry
     WHERE entry.source_session_id = checked_session
       AND entry.payload_kind = 'model_identity_changed'
       AND entry.model_identity_turn_id = checked_turn_id
       AND entry.model_identity_defaults_version = checked_defaults_version
       AND entry.model_identity_direct_selection_id = checked_selection;

    IF NOT boundary_required THEN
        RETURN boundary_entry_count = 0;
    END IF;

    IF predecessor_turn IS NULL
       OR predecessor_selection IS NOT DISTINCT FROM checked_selection
    THEN
        RETURN boundary_entry_count = 0;
    END IF;

    SELECT member_count
      INTO starting_member_count
      FROM context_frontier
     WHERE owning_session_id = checked_session
       AND context_frontier_id = checked_frontier_id;

    SELECT count(*), max(member.member_position)
      INTO boundary_member_count, boundary_member_position
      FROM semantic_transcript_entry AS entry
      JOIN context_frontier_member AS member
        ON member.source_session_id = entry.source_session_id
       AND member.semantic_entry_id = entry.semantic_entry_id
     WHERE entry.source_session_id = checked_session
       AND entry.payload_kind = 'model_identity_changed'
       AND entry.model_identity_turn_id = checked_turn_id
       AND member.owning_session_id = checked_session
       AND member.context_frontier_id = checked_frontier_id;

    RETURN boundary_entry_count = 1
       AND boundary_member_count = 1
       AND boundary_member_position IS NOT DISTINCT FROM
           starting_member_count - 1;
END;
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
          JOIN tool_attempt AS attempt
            ON attempt.request_id = request.request_id
           AND attempt.turn_id = request.turn_id
           AND attempt.session_id = request.session_id
          JOIN LATERAL turn_origin_exact_model_configuration(
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
           AND attempt.state_kind = 'terminal'
           AND attempt.terminal_disposition_kind = 'completed'
           AND attempt.effect_class = 'external_effect'
           AND attempt.result_content_kind = 'text'
           AND attempt.result_text::jsonb = jsonb_build_object(
                'result', 'session_spawned',
                'tool_request_id', relation.spawning_tool_request_id::text,
                'child_session_id', relation.child_session_id::text,
                'relationship', CASE relation.policy_kind
                    WHEN 'background' THEN jsonb_build_object('kind', 'background')
                    WHEN 'bound' THEN jsonb_build_object(
                        'kind', 'bound',
                        'on_parent_stopped', relation.on_parent_stopped,
                        'on_parent_cancelled', relation.on_parent_cancelled
                    )
                END
           )
           AND frozen.requested_model_kind = NEW.requested_model_kind
           AND frozen.requested_direct_model_selection_id IS NOT DISTINCT FROM
                NEW.requested_direct_model_selection_id
           AND frozen.requested_model_alias_id IS NOT DISTINCT FROM
                NEW.requested_model_alias_id
           AND frozen.frozen_model_kind = NEW.frozen_model_kind
           AND frozen.frozen_direct_model_selection_id IS NOT DISTINCT FROM
                NEW.frozen_direct_model_selection_id
           AND frozen.frozen_model_alias_id IS NOT DISTINCT FROM
                NEW.frozen_model_alias_id
           AND frozen.frozen_alias_selected_direct_id IS NOT DISTINCT FROM
                NEW.frozen_alias_selected_direct_id
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

-- Once a foreground wait resumes, the fresh current turn attempt directly
-- continues the ended attempt that issued the batch. Retain that exact history
-- without treating its already-ended physical attempts as foreign serial
-- authority.
DO $migration$
DECLARE
    lifecycle_definition text;
    updated_definition text;
    prior_attempt_check CONSTANT text := $old$
                               AND attempt.issuing_turn_attempt_id
                                   <> lifecycle.current_attempt_id
$old$;
    resumed_attempt_check CONSTANT text := $new$
                               AND attempt.issuing_turn_attempt_id
                                   <> lifecycle.current_attempt_id
                               AND NOT (
                                    EXISTS (
                                        SELECT 1
                                          FROM turn_attempt AS resumed
                                         WHERE resumed.turn_attempt_id =
                                               lifecycle.current_attempt_id
                                           AND resumed.turn_id = lifecycle.turn_id
                                           AND resumed.session_id = lifecycle.session_id
                                           AND resumed.continued_from_attempt_id =
                                               attempt.issuing_turn_attempt_id
                                    )
                                    AND EXISTS (
                                        SELECT 1
                                          FROM tool_attempt AS child_wait
                                          JOIN tool_request AS waited_request
                                            ON waited_request.request_id =
                                               child_wait.request_id
                                         WHERE waited_request.producing_model_call_id =
                                               lifecycle.active_tool_round_call_id
                                           AND child_wait.issuing_turn_attempt_id =
                                               attempt.issuing_turn_attempt_id
                                           AND child_wait.state_kind = 'terminal'
                                           AND child_wait.terminal_disposition_kind =
                                               'awaiting_child'
                                    )
                               )
$new$;
BEGIN
    SELECT pg_get_functiondef(
        'assert_tool_loop_turn_final_state_pre_delegation(uuid)'::regprocedure
    ) INTO lifecycle_definition;
    IF strpos(lifecycle_definition, prior_attempt_check) = 0 THEN
        RAISE EXCEPTION 'delegation could not locate serial authority assertion';
    END IF;
    updated_definition := replace(
        lifecycle_definition,
        prior_attempt_check,
        resumed_attempt_check
    );
    EXECUTE updated_definition;
END;
$migration$;

CREATE OR REPLACE FUNCTION assert_turn_attempt_final_state(
    checked_turn_attempt_id uuid
)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    attempt_record turn_attempt%ROWTYPE;
BEGIN
    SELECT *
      INTO attempt_record
      FROM turn_attempt
     WHERE turn_attempt_id = checked_turn_attempt_id;
    IF NOT FOUND OR attempt_record.continued_from_attempt_id IS NULL THEN
        RETURN;
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM turn_attempt AS predecessor
          JOIN model_call AS call
            ON call.turn_attempt_id = predecessor.turn_attempt_id
           AND call.turn_id = predecessor.turn_id
           AND call.session_id = predecessor.session_id
          JOIN tool_round AS round
            ON round.producing_model_call_id = call.model_call_id
           AND round.turn_id = call.turn_id
           AND round.session_id = call.session_id
         WHERE predecessor.turn_attempt_id =
               attempt_record.continued_from_attempt_id
           AND predecessor.turn_id = attempt_record.turn_id
           AND predecessor.session_id = attempt_record.session_id
           AND predecessor.state_kind = 'ended'
           AND predecessor.end_variant = 'without_stop'
           AND predecessor.end_disposition = 'yielded_to_durable_wait'
           AND call.state_kind = 'terminal'
           AND call.terminal_disposition_kind = 'completed'
           AND round.boundary_kind = 'continuing'
    ) AND NOT EXISTS (
        SELECT 1
          FROM turn_attempt AS predecessor
          JOIN tool_attempt AS child_wait_attempt
            ON child_wait_attempt.issuing_turn_attempt_id =
               predecessor.turn_attempt_id
           AND child_wait_attempt.turn_id = predecessor.turn_id
           AND child_wait_attempt.session_id = predecessor.session_id
          JOIN session_delegation_wait AS waiting
            ON waiting.awaiting_tool_request_id = child_wait_attempt.request_id
           AND waiting.spawning_tool_request_id =
               child_wait_attempt.wait_spawning_request_id
           AND waiting.child_session_id =
               child_wait_attempt.wait_child_session_id
           AND waiting.parent_turn_id = predecessor.turn_id
           AND waiting.parent_session_id = predecessor.session_id
           AND waiting.wait_mode = 'foreground'
          JOIN session_child_result_delivery AS delivery
            ON delivery.awaiting_tool_request_id =
               waiting.awaiting_tool_request_id
           AND delivery.spawning_tool_request_id =
               waiting.spawning_tool_request_id
           AND delivery.parent_session_id = waiting.parent_session_id
           AND delivery.delivery_sequence IS NULL
         WHERE predecessor.turn_attempt_id =
               attempt_record.continued_from_attempt_id
           AND predecessor.turn_id = attempt_record.turn_id
           AND predecessor.session_id = attempt_record.session_id
           AND predecessor.state_kind = 'ended'
           AND predecessor.end_variant = 'without_stop'
           AND predecessor.end_disposition = 'yielded_to_durable_wait'
           AND child_wait_attempt.state_kind = 'terminal'
           AND child_wait_attempt.terminal_disposition_kind = 'awaiting_child'
    ) THEN
        RAISE EXCEPTION
            'turn attempt continuation lacks an exact durable tool yield'
            USING
                ERRCODE = '23514',
                CONSTRAINT = 'turn_attempt_continuation_requires_tool_yield';
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

CREATE FUNCTION assert_delegation_parent_termination_chain(
    checked session_delegation_parent_termination,
    cascade session_delegation_termination_cascade
)
RETURNS void LANGUAGE plpgsql AS $$
BEGIN
    IF checked.command_source_kind <> cascade.root_source_kind
        OR (checked.command_source_kind = 'goal_command'
            AND checked.parent_goal_generation IS DISTINCT FROM
                cascade.root_goal_generation) THEN
        RAISE EXCEPTION 'delegation termination source contradicts its root command'
            USING ERRCODE = '23514',
                CONSTRAINT = 'session_delegation_parent_termination_chain';
    END IF;
    IF checked.source_kind = 'root' THEN
        IF checked.parent_session_id <> cascade.root_session_id
            OR checked.parent_turn_id IS DISTINCT FROM cascade.root_turn_id
            OR checked.parent_goal_generation IS DISTINCT FROM
                cascade.root_goal_generation
            OR checked.termination_kind <> cascade.termination_kind
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
           AND source_authority.root_command_id = checked.root_command_id
         WHERE source_event.spawning_tool_request_id =
                checked.source_spawning_tool_request_id
           AND source_event.event_kind = 'outcome_recorded'
           AND source_event.outcome_kind IN (
                'child_stopped', 'child_cancelled', 'already_terminal'
           )
           AND source_event.provenance_kind = CASE cascade.root_source_kind
                WHEN 'turn_command' THEN 'parent_turn_command'
                WHEN 'goal_command' THEN 'parent_goal_command'
           END
           AND source_event.provenance_command_id = checked.root_command_id
           AND source_event.provenance_turn_id IS NOT DISTINCT FROM
                source_authority.parent_turn_id
           AND source_event.provenance_goal_generation IS NOT DISTINCT FROM
                source_authority.parent_goal_generation
           AND source_relation.child_session_id = checked.parent_session_id
           AND (checked.command_source_kind = 'goal_command'
                OR source_task.turn_id = checked.parent_turn_id)
           AND checked.termination_kind = CASE source_event.outcome_kind
                WHEN 'child_stopped' THEN 'stopped'
                WHEN 'child_cancelled' THEN 'cancelled'
                WHEN 'already_terminal' THEN source_authority.termination_kind
           END
    ) THEN
        RAISE EXCEPTION 'nested delegation termination lacks its immediate parent outcome'
            USING ERRCODE = '23514',
                CONSTRAINT = 'session_delegation_parent_termination_chain';
    END IF;
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
    PERFORM assert_delegation_parent_termination_chain(NEW, cascade);
    RETURN NULL;
END;
$$;

CREATE FUNCTION require_delegation_cascade_parent_termination_chains()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    authority session_delegation_parent_termination%ROWTYPE;
BEGIN
    FOR authority IN
        SELECT stored.*
          FROM session_delegation_parent_termination AS stored
         WHERE stored.root_command_id = NEW.root_command_id
    LOOP
        PERFORM assert_delegation_parent_termination_chain(
            authority,
            NEW
        );
    END LOOP;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER session_delegation_termination_cascade_command
AFTER INSERT ON session_delegation_termination_cascade
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION require_delegation_termination_cascade_command();

CREATE CONSTRAINT TRIGGER session_delegation_cascade_requires_parent_chains
AFTER INSERT ON session_delegation_termination_cascade
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION require_delegation_cascade_parent_termination_chains();

CREATE CONSTRAINT TRIGGER session_delegation_parent_termination_chain
AFTER INSERT ON session_delegation_parent_termination
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION require_delegation_parent_termination_chain();

-- The forward cascade-to-command constraint above is intentionally paired
-- with these reverse command-to-cascade constraints. An applied
-- parent-and-descendants command may never commit as an implicit
-- parent-alone command merely because its scheduling writer omitted the
-- typed cascade proof.
CREATE FUNCTION require_applied_goal_command_delegation_cascade()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    applied_generation numeric(20, 0);
BEGIN
    IF NEW.operation_kind <> 'stop'
       OR NEW.result_kind <> 'applied'
       OR NEW.descendant_scope <> 'parent_and_descendants' THEN
        RETURN NULL;
    END IF;
    IF NOT EXISTS (
        SELECT 1 FROM session_delegation AS relation
         WHERE relation.parent_session_id = NEW.session_id
    ) THEN
        RETURN NULL;
    END IF;
    SELECT event.generation INTO applied_generation
      FROM goal_event AS event
     WHERE event.session_id = NEW.session_id
       AND event.event_ordinal = NEW.result_event_ordinal
       AND event.event_kind = 'user_stopped';
    IF applied_generation IS NULL OR NOT EXISTS (
        SELECT 1 FROM session_delegation_termination_cascade AS cascade
         WHERE cascade.root_command_id = NEW.command_id
           AND cascade.root_session_id = NEW.session_id
           AND cascade.root_source_kind = 'goal_command'
           AND cascade.root_turn_id IS NULL
           AND cascade.root_goal_generation = applied_generation
           AND cascade.termination_kind = 'stopped'
           AND cascade.descendant_scope = NEW.descendant_scope
    ) THEN
        RAISE EXCEPTION 'applied descendant-scoped goal command lacks its cascade proof'
            USING ERRCODE = '23514',
                CONSTRAINT = 'goal_command_delegation_cascade';
    END IF;
    RETURN NULL;
END;
$$;

CREATE FUNCTION require_applied_turn_command_delegation_cascade()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.delivery_kind <> 'interrupt'
       OR NEW.result_kind <> 'applied'
       OR NEW.descendant_scope <> 'parent_and_descendants' THEN
        RETURN NULL;
    END IF;
    IF NOT EXISTS (
        SELECT 1 FROM session_delegation AS relation
         WHERE relation.parent_session_id = NEW.session_id
    ) THEN
        RETURN NULL;
    END IF;
    IF NOT EXISTS (
        SELECT 1 FROM session_delegation_termination_cascade AS cascade
         WHERE cascade.root_command_id = NEW.command_id
           AND cascade.root_session_id = NEW.session_id
           AND cascade.root_source_kind = 'turn_command'
           AND cascade.root_turn_id = NEW.expected_active_turn_id
           AND cascade.root_goal_generation IS NULL
           AND cascade.termination_kind = 'cancelled'
           AND cascade.descendant_scope = NEW.descendant_scope
    ) THEN
        RAISE EXCEPTION 'applied descendant-scoped turn command lacks its cascade proof'
            USING ERRCODE = '23514',
                CONSTRAINT = 'submit_input_command_delegation_cascade';
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER applied_goal_command_requires_delegation_cascade
AFTER INSERT OR UPDATE ON goal_command
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION require_applied_goal_command_delegation_cascade();

CREATE CONSTRAINT TRIGGER applied_turn_command_requires_delegation_cascade
AFTER INSERT OR UPDATE ON submit_input_command
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION require_applied_turn_command_delegation_cascade();

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
LANGUAGE plpgsql
STABLE
AS $$
BEGIN
    RETURN QUERY WITH RECURSIVE frontier AS (
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
            END AS expected_action,
            ARRAY[
                relation.parent_session_id,
                relation.child_session_id
            ]::uuid[] AS visited_session_ids,
            relation.child_session_id <> relation.parent_session_id
                AS can_descend
          FROM session_delegation AS relation
         WHERE relation.parent_session_id = checked_root_session

        UNION ALL

        SELECT
            relation.spawning_tool_request_id,
            relation.parent_session_id,
            relation.child_session_id,
            CASE
                WHEN parent_result.spawning_tool_request_id IS NOT NULL THEN
                    parent.effective_parent_kind
                WHEN parent.expected_action = 'stop' THEN 'stopped'
                WHEN parent.expected_action = 'cancel' THEN 'cancelled'
            END AS effective_parent_kind,
            'parent_disposition'::text AS source_kind,
            parent.spawning_tool_request_id AS source_spawning_tool_request_id,
            CASE
                WHEN relation.policy_kind = 'background' THEN 'keep_running'
                WHEN parent_result.spawning_tool_request_id IS NOT NULL
                    AND parent.effective_parent_kind = 'stopped'
                    THEN relation.on_parent_stopped
                WHEN parent_result.spawning_tool_request_id IS NOT NULL
                    THEN relation.on_parent_cancelled
                WHEN parent.expected_action = 'stop' THEN relation.on_parent_stopped
                ELSE relation.on_parent_cancelled
            END AS expected_action,
            CASE
                WHEN relation.child_session_id = ANY(parent.visited_session_ids)
                    THEN parent.visited_session_ids
                ELSE parent.visited_session_ids || relation.child_session_id
            END AS visited_session_ids,
            NOT relation.child_session_id = ANY(parent.visited_session_ids)
                AS can_descend
          FROM frontier AS parent
          JOIN session_delegation AS relation
            ON relation.parent_session_id = parent.child_session_id
          LEFT JOIN session_child_result AS parent_result
            ON parent_result.spawning_tool_request_id =
                parent.spawning_tool_request_id
         WHERE (
                parent.expected_action IN ('stop', 'cancel')
                OR parent_result.spawning_tool_request_id IS NOT NULL
           )
           AND parent.can_descend
    )
    SELECT
        frontier.spawning_tool_request_id,
        frontier.parent_session_id,
        frontier.child_session_id,
        frontier.effective_parent_kind,
        frontier.source_kind,
        frontier.source_spawning_tool_request_id,
        frontier.expected_action
      FROM frontier;
END;
$$;

-- A descendant-scoped parent command takes the complete relationship frontier
-- before its repository can allocate any outbox sequence. Spawn admission takes
-- the same parent-session lock first, so the two writers never invert the
-- session/outbox order. Re-evaluate after waits because each statement gets a
-- fresh READ COMMITTED snapshot and may reveal a spawn that committed while a
-- parent lock was being acquired.
CREATE FUNCTION lock_delegation_termination_frontier(
    checked_root_session uuid,
    checked_root_kind text
)
RETURNS void LANGUAGE plpgsql AS $$
DECLARE
    prior_edge_count bigint := -1;
    current_edge_count bigint;
BEGIN
    LOOP
        PERFORM 1
          FROM session
         WHERE session_id IN (
            SELECT checked_root_session
            UNION
            SELECT frontier.parent_session_id
              FROM delegation_cascade_expected_frontier(
                    checked_root_session, checked_root_kind
              ) AS frontier
            UNION
            SELECT frontier.child_session_id
              FROM delegation_cascade_expected_frontier(
                    checked_root_session, checked_root_kind
              ) AS frontier
         )
         ORDER BY session_id
         FOR NO KEY UPDATE;

        SELECT count(*) INTO current_edge_count
          FROM delegation_cascade_expected_frontier(
                checked_root_session, checked_root_kind
          );
        EXIT WHEN current_edge_count = prior_edge_count;
        prior_edge_count := current_edge_count;
    END LOOP;

    PERFORM 1
      FROM session_delegation
     WHERE spawning_tool_request_id IN (
        SELECT frontier.spawning_tool_request_id
          FROM delegation_cascade_expected_frontier(
                checked_root_session, checked_root_kind
          ) AS frontier
     )
     ORDER BY spawning_tool_request_id
     FOR UPDATE;
END;
$$;

CREATE FUNCTION lock_delegation_frontier_before_goal_stop()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.operation_kind = 'stop'
        AND NEW.result_kind = 'applied'
        AND NEW.descendant_scope = 'parent_and_descendants'
    THEN
        PERFORM lock_delegation_termination_frontier(NEW.session_id, 'stopped');
    END IF;
    RETURN NEW;
END;
$$;

CREATE FUNCTION lock_delegation_frontier_before_input_interrupt()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.delivery_kind = 'interrupt'
        AND NEW.result_kind = 'applied'
        AND NEW.descendant_scope = 'parent_and_descendants'
    THEN
        PERFORM lock_delegation_termination_frontier(NEW.session_id, 'cancelled');
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER goal_command_locks_delegation_frontier
BEFORE INSERT ON goal_command
FOR EACH ROW EXECUTE FUNCTION lock_delegation_frontier_before_goal_stop();
CREATE TRIGGER submit_input_command_locks_delegation_frontier
BEFORE INSERT ON submit_input_command
FOR EACH ROW EXECUTE FUNCTION lock_delegation_frontier_before_input_interrupt();

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
     FOR NO KEY UPDATE;
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
            OR NOT EXISTS (
                SELECT 1
                  FROM tool_request AS request
                  JOIN tool_attempt AS attempt
                    ON attempt.request_id = request.request_id
                   AND attempt.turn_id = request.turn_id
                   AND attempt.session_id = request.session_id
                  JOIN session_message AS message
                    ON message.spawning_tool_request_id =
                        NEW.spawning_tool_request_id
                   AND message.event_ordinal = NEW.event_ordinal
                  JOIN session_message_delivery AS delivery
                    ON delivery.message_id = message.message_id
                   AND delivery.spawning_tool_request_id =
                        message.spawning_tool_request_id
                 WHERE request.request_id = NEW.provenance_tool_request_id
                   AND request.session_id = NEW.provenance_session_id
                   AND request.tool_name = 'send_session_message'
                   AND request.arguments_kind = 'json'
                   AND request.arguments_text::jsonb = jsonb_build_object(
                        'content', stored_content,
                        'peer_session_id', CASE stored_direction
                            WHEN 'parent_to_child' THEN relation_child::text
                            WHEN 'child_to_parent' THEN relation_parent::text
                        END
                   )
                   AND attempt.state_kind = 'terminal'
                   AND attempt.terminal_disposition_kind = 'completed'
                   AND attempt.effect_class = 'external_effect'
                   AND attempt.result_content_kind = 'text'
                   AND attempt.result_text::jsonb = jsonb_build_object(
                        'result', 'session_message_sent',
                        'tool_request_id', request.request_id::text,
                        'message_id', message.message_id::text,
                        'direction', message.direction,
                        'ordinal', message.event_ordinal,
                        'delivery_sequence', delivery.delivery_sequence
                   )
            )
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
            IF NEW.outcome_kind IN ('child_stopped', 'child_cancelled')
                AND NOT EXISTS (
                    SELECT 1
                      FROM session_delegation_initial_task AS task
                      JOIN turn_lifecycle AS lifecycle
                        ON lifecycle.turn_id = task.turn_id
                       AND lifecycle.session_id = task.child_session_id
                       AND lifecycle.state_kind = 'terminal'
                       AND lifecycle.terminal_disposition_kind = 'cancelled'
                      JOIN semantic_transcript_entry AS marker
                        ON marker.source_session_id = lifecycle.session_id
                       AND marker.payload_kind = 'turn_cancelled'
                       AND marker.cancelled_turn_id = lifecycle.turn_id
                      JOIN context_frontier AS frontier
                        ON frontier.owning_session_id = lifecycle.session_id
                       AND frontier.context_frontier_id =
                            lifecycle.terminal_frontier_id
                      JOIN context_frontier_member AS terminal_member
                        ON terminal_member.owning_session_id =
                            frontier.owning_session_id
                       AND terminal_member.context_frontier_id =
                            frontier.context_frontier_id
                       AND terminal_member.member_position = frontier.member_count
                       AND terminal_member.source_session_id =
                            marker.source_session_id
                       AND terminal_member.semantic_entry_id =
                            marker.semantic_entry_id
                      JOIN turn_cancelled_outbox_event AS cancellation
                        ON cancellation.session_id = lifecycle.session_id
                       AND cancellation.turn_id = lifecycle.turn_id
                       AND cancellation.cancellation_entry_id =
                            marker.semantic_entry_id
                       AND cancellation.terminal_frontier_id =
                            lifecycle.terminal_frontier_id
                     WHERE task.spawning_tool_request_id =
                            NEW.spawning_tool_request_id
                       AND task.child_session_id = relation_child
                ) THEN
                RAISE EXCEPTION 'parent disposition lacks exact child terminal evidence'
                    USING ERRCODE = '23514',
                        CONSTRAINT = 'session_delegation_event_semantics';
            END IF;
        ELSE
            RAISE EXCEPTION 'outcome reason is not a delegation descendant event'
                USING ERRCODE = '23514', CONSTRAINT = 'session_delegation_event_semantics';
        END IF;
    END IF;
    RETURN NULL;
END;
$$;

-- A delegated child turn cannot terminalize without atomically materializing
-- its typed relationship outcome. The event-side validator above proves the
-- forward correlation; this reverse deferred check prevents a terminal child
-- transition from omitting the event, immutable result, deliveries, and wakes.
CREATE FUNCTION require_terminal_delegated_turn_result()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    checked_turn uuid;
    checked_session uuid;
    spawning_request uuid;
BEGIN
    checked_turn := COALESCE(NEW.turn_id, OLD.turn_id);
    IF TG_TABLE_NAME = 'turn_lifecycle' THEN
        checked_session := COALESCE(NEW.session_id, OLD.session_id);
    ELSE
        checked_session := COALESCE(
            NEW.child_session_id,
            OLD.child_session_id
        );
    END IF;
    SELECT task.spawning_tool_request_id
      INTO spawning_request
      FROM session_delegation_initial_task AS task
     WHERE task.turn_id = checked_turn
       AND task.child_session_id = checked_session;
    IF NOT FOUND THEN
        RETURN NULL;
    END IF;
    IF EXISTS (
        SELECT 1 FROM turn_lifecycle AS lifecycle
         WHERE lifecycle.turn_id = checked_turn
           AND lifecycle.session_id = checked_session
           AND lifecycle.state_kind = 'terminal'
    ) AND (SELECT count(*) FROM session_child_result AS result
            WHERE result.spawning_tool_request_id = spawning_request) <> 1 THEN
        RAISE EXCEPTION 'terminal delegated child turn requires exactly one typed result'
            USING ERRCODE = '23503',
                CONSTRAINT = 'terminal_delegated_turn_result_required';
    END IF;
    RETURN NULL;
END;
$$;
CREATE CONSTRAINT TRIGGER turn_lifecycle_zz_requires_delegated_result
AFTER INSERT OR UPDATE OR DELETE ON turn_lifecycle
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION require_terminal_delegated_turn_result();
CREATE CONSTRAINT TRIGGER delegation_initial_task_zz_requires_terminal_result
AFTER INSERT OR UPDATE OR DELETE ON session_delegation_initial_task
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION require_terminal_delegated_turn_result();

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
    IF NEW.wait_mode = 'background' AND NOT EXISTS (
        SELECT 1 FROM tool_attempt
         WHERE request_id = NEW.awaiting_tool_request_id
           AND session_id = NEW.parent_session_id
           AND turn_id = NEW.parent_turn_id
           AND state_kind = 'terminal'
           AND terminal_disposition_kind = 'completed'
           AND effect_class = 'effect_free'
           AND result_content_kind = 'text'
           AND result_text::jsonb = jsonb_build_object(
                'result', 'session_await_registered',
                'tool_request_id', NEW.awaiting_tool_request_id::text,
                'child_session_id', NEW.child_session_id::text,
                'mode', 'background'
           )
    ) THEN
        RAISE EXCEPTION 'background delegation wait requires exact completed registration receipt'
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
     WHERE session_id = NEW.recipient_session_id FOR NO KEY UPDATE;
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
        OR (delivery_sequence IS NOT NULL AND delivery_kind IS NOT NULL
            AND delivery_kind = 'background_result')
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

-- An idle-recipient wake is a distinct typed turn origin. The closed delivery
-- range is the content appended after the predecessor frontier; the final item
-- is the logical origin entry used by the generic lifecycle validators.
CREATE TABLE session_delegation_wake_turn_origin (
    turn_id uuid PRIMARY KEY,
    recipient_session_id uuid NOT NULL,
    admission_position numeric(20, 0) NOT NULL CHECK (
        admission_position BETWEEN 1 AND 18446744073709551615
    ),
    first_delivery_sequence numeric(20, 0) NOT NULL CHECK (
        first_delivery_sequence BETWEEN 1 AND 18446744073709551615
    ),
    through_delivery_sequence numeric(20, 0) NOT NULL CHECK (
        through_delivery_sequence BETWEEN 1 AND 18446744073709551615
    ),
    defaults_version numeric(20, 0) NOT NULL CHECK (
        defaults_version BETWEEN 1 AND 18446744073709551615
    ),
    requested_model_kind text NOT NULL,
    requested_direct_model_selection_id uuid,
    requested_model_alias_id uuid,
    frozen_model_kind text NOT NULL,
    frozen_direct_model_selection_id uuid,
    frozen_model_alias_id uuid,
    frozen_alias_selected_direct_id uuid,
    CONSTRAINT session_delegation_wake_delivery_range CHECK (
        first_delivery_sequence <= through_delivery_sequence
    ),
    CONSTRAINT session_delegation_wake_requested_model_shape CHECK (
        (requested_model_kind = 'direct'
            AND requested_direct_model_selection_id IS NOT NULL
            AND requested_model_alias_id IS NULL)
        OR (requested_model_kind = 'alias'
            AND requested_direct_model_selection_id IS NULL
            AND requested_model_alias_id IS NOT NULL)
    ),
    CONSTRAINT session_delegation_wake_frozen_model_shape CHECK (
        (frozen_model_kind = 'direct'
            AND frozen_direct_model_selection_id IS NOT NULL
            AND frozen_model_alias_id IS NULL
            AND frozen_alias_selected_direct_id IS NULL)
        OR (frozen_model_kind = 'frozen_alias'
            AND frozen_direct_model_selection_id IS NULL
            AND frozen_model_alias_id IS NOT NULL
            AND frozen_alias_selected_direct_id IS NOT NULL)
    ),
    UNIQUE (turn_id, recipient_session_id, admission_position),
    UNIQUE (recipient_session_id, through_delivery_sequence),
    FOREIGN KEY (turn_id, recipient_session_id, admission_position)
        REFERENCES turn_lifecycle(turn_id, session_id, acceptance_position)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (recipient_session_id, through_delivery_sequence)
        REFERENCES session_pending_delivery(
            recipient_session_id, delivery_sequence
        ) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (recipient_session_id, defaults_version)
        REFERENCES session_defaults_version(session_id, version)
        ON UPDATE RESTRICT ON DELETE RESTRICT
);

CREATE FUNCTION guard_session_delegation_wake_turn_origin_change()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'delegation wake turn origin is not deletable'
            USING ERRCODE = '23514';
    END IF;
    IF ROW(
        OLD.turn_id,
        OLD.recipient_session_id,
        OLD.admission_position,
        OLD.first_delivery_sequence,
        OLD.defaults_version,
        OLD.requested_model_kind,
        OLD.requested_direct_model_selection_id,
        OLD.requested_model_alias_id,
        OLD.frozen_model_kind,
        OLD.frozen_direct_model_selection_id,
        OLD.frozen_model_alias_id,
        OLD.frozen_alias_selected_direct_id
    ) IS DISTINCT FROM ROW(
        NEW.turn_id,
        NEW.recipient_session_id,
        NEW.admission_position,
        NEW.first_delivery_sequence,
        NEW.defaults_version,
        NEW.requested_model_kind,
        NEW.requested_direct_model_selection_id,
        NEW.requested_model_alias_id,
        NEW.frozen_model_kind,
        NEW.frozen_direct_model_selection_id,
        NEW.frozen_model_alias_id,
        NEW.frozen_alias_selected_direct_id
    ) OR NEW.through_delivery_sequence <= OLD.through_delivery_sequence
      OR NOT EXISTS (
            SELECT 1 FROM turn_lifecycle AS lifecycle
             WHERE lifecycle.turn_id = OLD.turn_id
               AND lifecycle.session_id = OLD.recipient_session_id
               AND lifecycle.state_kind = 'queued'
      ) THEN
        RAISE EXCEPTION 'only a queued wake origin may extend its delivery range'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER session_delegation_wake_turn_origin_changes_are_guarded
BEFORE UPDATE OR DELETE ON session_delegation_wake_turn_origin
FOR EACH ROW EXECUTE FUNCTION guard_session_delegation_wake_turn_origin_change();

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
            'tool_result_request_id',
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
                    AND tool_result_request_id IS NULL
                    AND delegation_result_awaiting_tool_request_id IS NULL
                    AND delegation_result_spawning_tool_request_id IS NULL
                    AND %s)
                 OR (payload_kind = ''delegation_result''
                    AND delegation_message_id IS NULL
                    AND delegation_result_awaiting_tool_request_id IS NOT NULL
                    AND (
                        tool_result_request_id IS NULL
                        OR tool_result_request_id =
                            delegation_result_awaiting_tool_request_id
                    )
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

-- A delivered foreground child result is the logical result of its exact
-- await_session request. Extend the pre-existing tool-loop validators rather
-- than inventing a parallel result-ordering rule.
DO $migration$
DECLARE
    checked_function regprocedure;
    definition text;
    updated_definition text;
BEGIN
    FOREACH checked_function IN ARRAY ARRAY[
        'assert_tool_request_single_result(uuid)'::regprocedure,
        'assert_tool_loop_turn_final_state_pre_delegation(uuid)'::regprocedure,
        'assert_reconciliation_required_turn_final_state(uuid)'::regprocedure,
        'continuation_frontier_closes_predecessor_tool_round(uuid,uuid,uuid,uuid)'::regprocedure
    ]
    LOOP
        SELECT pg_get_functiondef(checked_function) INTO definition;
        updated_definition := regexp_replace(
            definition,
            '''tool_execution_result''[[:space:]]*,[[:space:]]*''tool_denied''[[:space:]]*,[[:space:]]*''tool_closed_by_turn_end''',
            '''tool_execution_result'', ''tool_denied'', ''tool_closed_by_turn_end'', ''delegation_result''',
            'g'
        );
        IF updated_definition = definition THEN
            RAISE EXCEPTION
                'delegation result extension could not find tool-result vocabulary in %',
                checked_function;
        END IF;
        EXECUTE updated_definition;
    END LOOP;

    SELECT pg_get_functiondef(
        'assert_model_call_steering_final_state(uuid)'::regprocedure
    ) INTO definition;
    updated_definition := replace(
        definition,
        'entry.payload_kind = ''tool_denied''',
        'entry.payload_kind IN (''tool_denied'', ''delegation_result'')'
    );
    IF updated_definition = definition THEN
        RAISE EXCEPTION
            'delegation result extension could not find continued-call result arm';
    END IF;
    EXECUTE updated_definition;
END;
$migration$;

-- A foreground child result closes one ordinary tool exchange for compaction.
-- Background results remain inbox content and never satisfy a tool-use count.
DO $migration$
DECLARE
    definition text;
    updated_definition text;
    replacement_count bigint;
BEGIN
    SELECT pg_get_functiondef(
        'require_context_compaction_exact_evidence()'::regprocedure
    ) INTO definition;
    updated_definition := regexp_replace(
        definition,
        'entry\.payload_kind IN \([[:space:]]*''tool_execution_result''[[:space:]]*,[[:space:]]*''tool_denied''[[:space:]]*,[[:space:]]*''tool_closed_by_turn_end''[[:space:]]*\)',
        '(entry.payload_kind IN (''tool_execution_result'', ''tool_denied'', ''tool_closed_by_turn_end'') OR (entry.payload_kind = ''delegation_result'' AND entry.tool_result_request_id IS NOT NULL))',
        'g'
    );
    IF updated_definition = definition THEN
        RAISE EXCEPTION
            'delegation result extension could not find compaction result counts';
    END IF;
    SELECT count(*) INTO replacement_count
      FROM regexp_matches(
            updated_definition,
            'entry\.payload_kind = ''delegation_result'' AND entry\.tool_result_request_id IS NOT NULL',
            'g'
      );
    IF replacement_count <> 2 THEN
        RAISE EXCEPTION
            'delegation result extension expected two compaction result counts';
    END IF;
    EXECUTE updated_definition;
END;
$migration$;

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

CREATE FUNCTION delegation_delivery_semantic_entry(
    checked_recipient uuid,
    checked_sequence numeric(20, 0)
)
RETURNS uuid
LANGUAGE sql
STABLE
AS $$
    SELECT entry.semantic_entry_id
      FROM session_pending_delivery AS pending
      JOIN session_message_delivery AS delivery
        ON pending.delivery_kind = 'message'
       AND delivery.recipient_session_id = pending.recipient_session_id
       AND delivery.delivery_sequence = pending.delivery_sequence
      JOIN semantic_transcript_entry AS entry
        ON entry.source_session_id = delivery.recipient_session_id
       AND entry.payload_kind = 'delegation_message'
       AND entry.delegation_message_id = delivery.message_id
     WHERE pending.recipient_session_id = checked_recipient
       AND pending.delivery_sequence = checked_sequence
    UNION ALL
    SELECT entry.semantic_entry_id
      FROM session_pending_delivery AS pending
      JOIN session_child_result_delivery AS delivery
        ON pending.delivery_kind = 'background_result'
       AND delivery.parent_session_id = pending.recipient_session_id
       AND delivery.delivery_sequence = pending.delivery_sequence
      JOIN semantic_transcript_entry AS entry
        ON entry.source_session_id = delivery.parent_session_id
       AND entry.payload_kind = 'delegation_result'
       AND entry.delegation_result_awaiting_tool_request_id =
            delivery.awaiting_tool_request_id
       AND entry.delegation_result_spawning_tool_request_id =
            delivery.spawning_tool_request_id
     WHERE pending.recipient_session_id = checked_recipient
       AND pending.delivery_sequence = checked_sequence
$$;

CREATE FUNCTION model_call_suffix_entry_is_valid(
    checked_session uuid,
    checked_turn uuid,
    checked_model_call uuid,
    checked_source_session uuid,
    checked_entry uuid
)
RETURNS boolean
LANGUAGE sql
STABLE
AS $$
    SELECT checked_source_session = checked_session AND (
        EXISTS (
            SELECT 1
              FROM semantic_transcript_entry AS entry
              JOIN accepted_input AS accepted
                ON accepted.accepted_input_id = entry.origin_accepted_input_id
               AND accepted.session_id = entry.source_session_id
             WHERE entry.source_session_id = checked_source_session
               AND entry.semantic_entry_id = checked_entry
               AND entry.payload_kind = 'steering_accepted_input'
               AND entry.steering_source_turn_id = checked_turn
               AND accepted.disposition_kind = 'consumed_as_steering'
               AND accepted.expected_active_turn_id = checked_turn
               AND accepted.consuming_model_call_id = checked_model_call
        ) OR EXISTS (
            SELECT 1
              FROM session_pending_delivery AS pending
             WHERE pending.recipient_session_id = checked_session
               AND delegation_delivery_semantic_entry(
                    pending.recipient_session_id,
                    pending.delivery_sequence
               ) = checked_entry
        )
    )
$$;

CREATE FUNCTION model_call_delivery_suffix_is_ordered(
    checked_session uuid,
    checked_frontier uuid,
    suffix_start numeric(20, 0)
)
RETURNS boolean
LANGUAGE sql
STABLE
AS $$
    SELECT NOT EXISTS (
        SELECT 1
          FROM (
                SELECT
                    pending.delivery_sequence,
                    row_number() OVER (
                        ORDER BY pending.delivery_sequence
                    ) AS delivery_order,
                    row_number() OVER (
                        ORDER BY member.member_position
                    ) AS member_order
                  FROM context_frontier_member AS member
                  JOIN session_pending_delivery AS pending
                    ON pending.recipient_session_id = checked_session
                   AND delegation_delivery_semantic_entry(
                        pending.recipient_session_id,
                        pending.delivery_sequence
                   ) = member.semantic_entry_id
                 WHERE member.owning_session_id = checked_session
                   AND member.context_frontier_id = checked_frontier
                   AND member.member_position > suffix_start
                   AND member.source_session_id = checked_session
          ) AS ordered
         WHERE ordered.delivery_order <> ordered.member_order
    )
$$;

-- Continuation calls consume the recipient's entire as-of-commit delegation
-- inbox alongside pending steering. The existing validator still owns the
-- safe-point boundary; these substitutions extend only its suffix vocabulary,
-- completeness count, and recipient-wide delivery ordering.
DO $migration$
DECLARE
    definition text;
    updated_definition text;
BEGIN
    SELECT pg_get_functiondef(
        'assert_model_call_steering_final_state(uuid)'::regprocedure
    ) INTO definition;

    updated_definition := regexp_replace(
        definition,
        'AND consuming_model_call_id = checked_model_call_id;[[:space:]]+SELECT count\(\*\)[[:space:]]+INTO malformed_count',
        $replacement$AND consuming_model_call_id = checked_model_call_id;

    SELECT consumed_count + count(*)
      INTO consumed_count
      FROM session_pending_delivery AS pending
     WHERE pending.recipient_session_id = checked_session
       AND NOT EXISTS (
            SELECT 1
              FROM context_frontier_member AS prior_member
             WHERE prior_member.owning_session_id = checked_session
               AND prior_member.context_frontier_id = checked_frontier
               AND prior_member.member_position <= suffix_start_count
               AND prior_member.source_session_id = checked_session
               AND prior_member.semantic_entry_id =
                    delegation_delivery_semantic_entry(
                        pending.recipient_session_id,
                        pending.delivery_sequence
                    )
       );

    SELECT count(*)
      INTO malformed_count$replacement$
    );
    IF updated_definition = definition THEN
        RAISE EXCEPTION
            'delegation delivery extension could not augment suffix completeness';
    END IF;
    definition := updated_definition;

    updated_definition := replace(
        definition,
        $search$AND (
            entry.payload_kind IS DISTINCT FROM 'steering_accepted_input'
            OR entry.source_session_id IS DISTINCT FROM checked_session
            OR entry.steering_source_turn_id IS DISTINCT FROM checked_turn
            OR accepted.disposition_kind IS DISTINCT FROM
               'consumed_as_steering'
            OR accepted.expected_active_turn_id IS DISTINCT FROM checked_turn
            OR accepted.consuming_model_call_id IS DISTINCT FROM
               checked_model_call_id
       )$search$,
        $replacement$AND NOT model_call_suffix_entry_is_valid(
            checked_session,
            checked_turn,
            checked_model_call_id,
            entry.source_session_id,
            entry.semantic_entry_id
       )$replacement$
    );
    IF updated_definition = definition THEN
        RAISE EXCEPTION
            'delegation delivery extension could not widen suffix vocabulary';
    END IF;
    definition := updated_definition;

    updated_definition := replace(
        definition,
        $search$OR malformed_count <> 0
       OR EXISTS ($search$,
        $replacement$OR malformed_count <> 0
       OR NOT model_call_delivery_suffix_is_ordered(
            checked_session,
            checked_frontier,
            suffix_start_count
       )
       OR EXISTS ($replacement$
    );
    IF updated_definition = definition THEN
        RAISE EXCEPTION
            'delegation delivery extension could not add FIFO ordering';
    END IF;
    EXECUTE updated_definition;
END;
$migration$;

CREATE OR REPLACE FUNCTION require_delegation_initial_task_origin()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    delegation_origin_count bigint;
BEGIN
    SELECT
        (SELECT count(*) FROM session_delegation_initial_task AS task
          WHERE task.turn_id = NEW.turn_id
            AND task.child_session_id = NEW.session_id
            AND task.admission_position = NEW.acceptance_position)
        +
        (SELECT count(*) FROM session_delegation_wake_turn_origin AS wake
          WHERE wake.turn_id = NEW.turn_id
            AND wake.recipient_session_id = NEW.session_id
            AND wake.admission_position = NEW.acceptance_position)
      INTO delegation_origin_count;
    IF (NEW.origin_kind = 'delegation') IS DISTINCT FROM
            (delegation_origin_count = 1)
       OR delegation_origin_count > 1 THEN
        RAISE EXCEPTION 'turn lifecycle requires exactly its typed origin'
            USING ERRCODE = '23514',
                CONSTRAINT = 'turn_lifecycle_typed_origin';
    END IF;
    RETURN NULL;
END;
$$;

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
                COALESCE(
                    task.frozen_direct_model_selection_id,
                    task.frozen_alias_selected_direct_id
                ),
                ARRAY[task.turn_id]::uuid[] AS visited_turn_ids
              FROM session_delegation_initial_task AS task
             WHERE task.turn_id = checked_turn_id
               AND task.child_session_id = checked_session_id

            UNION ALL

            SELECT
                wake.turn_id,
                NULL::uuid AS source_configuration_turn_id,
                wake.defaults_version,
                COALESCE(
                    wake.frozen_direct_model_selection_id,
                    wake.frozen_alias_selected_direct_id
                ),
                ARRAY[wake.turn_id]::uuid[] AS visited_turn_ids
              FROM session_delegation_wake_turn_origin AS wake
             WHERE wake.turn_id = checked_turn_id
               AND wake.recipient_session_id = checked_session_id
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

CREATE OR REPLACE FUNCTION turn_origin_exact_model_configuration(
    checked_turn_id uuid,
    checked_session_id uuid
)
RETURNS TABLE (
    defaults_version numeric(20, 0),
    requested_model_kind text,
    requested_direct_model_selection_id uuid,
    requested_model_alias_id uuid,
    frozen_model_kind text,
    frozen_direct_model_selection_id uuid,
    frozen_model_alias_id uuid,
    frozen_alias_selected_direct_id uuid
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
                origin.requested_model_kind,
                origin.requested_direct_model_selection_id,
                origin.requested_model_alias_id,
                origin.frozen_model_kind,
                origin.frozen_direct_model_selection_id,
                origin.frozen_model_alias_id,
                origin.frozen_alias_selected_direct_id,
                ARRAY[origin.turn_id]::uuid[] AS visited_turn_ids
              FROM queued_input_origin AS origin
             WHERE origin.turn_id = checked_turn_id
               AND origin.session_id = checked_session_id

            UNION ALL

            SELECT
                task.turn_id,
                NULL::uuid,
                task.defaults_version,
                task.requested_model_kind,
                task.requested_direct_model_selection_id,
                task.requested_model_alias_id,
                task.frozen_model_kind,
                task.frozen_direct_model_selection_id,
                task.frozen_model_alias_id,
                task.frozen_alias_selected_direct_id,
                ARRAY[task.turn_id]::uuid[]
              FROM session_delegation_initial_task AS task
             WHERE task.turn_id = checked_turn_id
               AND task.child_session_id = checked_session_id

            UNION ALL

            SELECT
                wake.turn_id,
                NULL::uuid,
                wake.defaults_version,
                wake.requested_model_kind,
                wake.requested_direct_model_selection_id,
                wake.requested_model_alias_id,
                wake.frozen_model_kind,
                wake.frozen_direct_model_selection_id,
                wake.frozen_model_alias_id,
                wake.frozen_alias_selected_direct_id,
                ARRAY[wake.turn_id]::uuid[]
              FROM session_delegation_wake_turn_origin AS wake
             WHERE wake.turn_id = checked_turn_id
               AND wake.recipient_session_id = checked_session_id
        )

        UNION ALL

        SELECT
            source.turn_id,
            source.source_configuration_turn_id,
            source.defaults_version,
            source.requested_model_kind,
            source.requested_direct_model_selection_id,
            source.requested_model_alias_id,
            source.frozen_model_kind,
            source.frozen_direct_model_selection_id,
            source.frozen_model_alias_id,
            source.frozen_alias_selected_direct_id,
            chain.visited_turn_ids || source.turn_id
          FROM configuration_chain AS chain
          JOIN queued_input_origin AS source
            ON source.turn_id = chain.source_configuration_turn_id
           AND source.session_id = checked_session_id
         WHERE NOT source.turn_id = ANY(chain.visited_turn_ids)
    )
    SELECT
        chain.defaults_version,
        chain.requested_model_kind,
        chain.requested_direct_model_selection_id,
        chain.requested_model_alias_id,
        chain.frozen_model_kind,
        chain.frozen_direct_model_selection_id,
        chain.frozen_model_alias_id,
        chain.frozen_alias_selected_direct_id
      FROM configuration_chain AS chain
     WHERE chain.defaults_version IS NOT NULL
       AND chain.requested_model_kind IS NOT NULL
       AND chain.frozen_model_kind IS NOT NULL
     ORDER BY cardinality(chain.visited_turn_ids) DESC
     LIMIT 1
$$;

CREATE OR REPLACE FUNCTION turn_lifecycle_origin_semantic_entry(
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
      LEFT JOIN session_delegation_wake_turn_origin AS wake
        ON wake.turn_id = lifecycle.turn_id
       AND wake.recipient_session_id = lifecycle.session_id
       AND delegation_delivery_semantic_entry(
            wake.recipient_session_id,
            wake.through_delivery_sequence
       ) = entry.semantic_entry_id
     WHERE lifecycle.turn_id = checked_turn_id
       AND lifecycle.session_id = checked_session_id
       AND (
            (lifecycle.origin_kind = 'accepted_input'
                AND entry.payload_kind = 'origin_accepted_input'
                AND entry.origin_accepted_input_id = checked_origin_input_id)
            OR (lifecycle.origin_kind = 'delegation'
                AND lifecycle.state_kind <> 'queued'
                AND (
                    (entry.payload_kind = 'delegated_task'
                        AND task.spawning_tool_request_id =
                            entry.delegated_task_spawning_tool_request_id)
                    OR wake.turn_id IS NOT NULL
                ))
       )
$$;

CREATE OR REPLACE FUNCTION accepted_input_turn_queue_predecessor(
    checked_session uuid,
    checked_turn uuid
)
RETURNS uuid
LANGUAGE sql
STABLE
AS $$
    WITH RECURSIVE derived_order (
        turn_id,
        root_position,
        interrupt_depth
    ) AS (
        SELECT
            lifecycle.turn_id,
            lifecycle.acceptance_position,
            0::bigint
          FROM turn_lifecycle AS lifecycle
          LEFT JOIN queued_input_origin AS origin
            ON origin.turn_id = lifecycle.turn_id
           AND origin.session_id = lifecycle.session_id
         WHERE lifecycle.session_id = checked_session
           AND (
                origin.priority_kind = 'ordinary'
                OR (
                    lifecycle.origin_kind = 'delegation'
                    AND (
                        EXISTS (
                            SELECT 1
                              FROM session_delegation_initial_task AS task
                             WHERE task.turn_id = lifecycle.turn_id
                               AND task.child_session_id = lifecycle.session_id
                        )
                        OR EXISTS (
                            SELECT 1
                              FROM session_delegation_wake_turn_origin AS wake
                             WHERE wake.turn_id = lifecycle.turn_id
                               AND wake.recipient_session_id = lifecycle.session_id
                        )
                    )
                )
           )
           AND goal_turn_is_runtime_relevant(
                lifecycle.session_id,
                lifecycle.turn_id
           )
        UNION ALL
        SELECT
            successor.turn_id,
            predecessor.root_position,
            predecessor.interrupt_depth + 1
          FROM derived_order AS predecessor
          JOIN queued_input_origin AS successor
            ON successor.session_id = checked_session
           AND successor.priority_kind = 'interrupt_immediately_after'
           AND successor.interrupt_predecessor_turn_id = predecessor.turn_id
          JOIN turn_lifecycle AS successor_lifecycle
            ON successor_lifecycle.turn_id = successor.turn_id
           AND successor_lifecycle.session_id = successor.session_id
         WHERE goal_turn_is_runtime_relevant(
            successor_lifecycle.session_id,
            successor_lifecycle.turn_id
         )
    ),
    ranked AS (
        SELECT
            turn_id,
            lag(turn_id) OVER (
                ORDER BY root_position, interrupt_depth
            ) AS predecessor_turn
          FROM derived_order
    )
    SELECT predecessor_turn
      FROM ranked
     WHERE turn_id = checked_turn
$$;

CREATE OR REPLACE FUNCTION accepted_input_turn_is_first_nonterminal(
    checked_session uuid,
    checked_turn uuid
)
RETURNS boolean
LANGUAGE sql
STABLE
AS $$
    WITH RECURSIVE derived_order (
        turn_id,
        root_position,
        interrupt_depth
    ) AS (
        SELECT
            lifecycle.turn_id,
            lifecycle.acceptance_position,
            0::bigint
          FROM turn_lifecycle AS lifecycle
          LEFT JOIN queued_input_origin AS origin
            ON origin.turn_id = lifecycle.turn_id
           AND origin.session_id = lifecycle.session_id
         WHERE lifecycle.session_id = checked_session
           AND (
                origin.priority_kind = 'ordinary'
                OR (
                    lifecycle.origin_kind = 'delegation'
                    AND (
                        EXISTS (
                            SELECT 1
                              FROM session_delegation_initial_task AS task
                             WHERE task.turn_id = lifecycle.turn_id
                               AND task.child_session_id = lifecycle.session_id
                        )
                        OR EXISTS (
                            SELECT 1
                              FROM session_delegation_wake_turn_origin AS wake
                             WHERE wake.turn_id = lifecycle.turn_id
                               AND wake.recipient_session_id = lifecycle.session_id
                        )
                    )
                )
           )
           AND goal_turn_is_runtime_relevant(
                lifecycle.session_id,
                lifecycle.turn_id
           )
        UNION ALL
        SELECT
            successor.turn_id,
            predecessor.root_position,
            predecessor.interrupt_depth + 1
          FROM derived_order AS predecessor
          JOIN queued_input_origin AS successor
            ON successor.session_id = checked_session
           AND successor.priority_kind = 'interrupt_immediately_after'
           AND successor.interrupt_predecessor_turn_id = predecessor.turn_id
          JOIN turn_lifecycle AS successor_lifecycle
            ON successor_lifecycle.turn_id = successor.turn_id
           AND successor_lifecycle.session_id = successor.session_id
         WHERE goal_turn_is_runtime_relevant(
            successor_lifecycle.session_id,
            successor_lifecycle.turn_id
         )
    ),
    ranked AS (
        SELECT
            turn_id,
            row_number() OVER (
                ORDER BY root_position, interrupt_depth
            ) AS queue_rank
          FROM derived_order
    ),
    candidate AS (
        SELECT queue_rank
          FROM ranked
         WHERE turn_id = checked_turn
    )
    SELECT EXISTS (SELECT 1 FROM candidate)
       AND NOT EXISTS (
            SELECT 1
              FROM ranked AS earlier
              JOIN turn_lifecycle AS lifecycle
                ON lifecycle.turn_id = earlier.turn_id
               AND lifecycle.session_id = checked_session
              JOIN candidate
                ON earlier.queue_rank < candidate.queue_rank
             WHERE lifecycle.state_kind <> 'terminal'
       )
$$;

CREATE FUNCTION turn_lifecycle_origin_member_span(
    checked_turn_id uuid,
    checked_session_id uuid
)
RETURNS numeric(20, 0)
LANGUAGE sql
STABLE
AS $$
    SELECT COALESCE(
        (
            SELECT wake.through_delivery_sequence
                   - wake.first_delivery_sequence + 1
              FROM session_delegation_wake_turn_origin AS wake
             WHERE wake.turn_id = checked_turn_id
               AND wake.recipient_session_id = checked_session_id
        ),
        1
    )
$$;

DO $migration$
DECLARE
    checked_function regprocedure;
    definition text;
    updated_definition text;
BEGIN
    FOREACH checked_function IN ARRAY ARRAY[
        'assert_turn_lifecycle_final_state_without_steering(uuid)'::regprocedure,
        'assert_terminal_started_turn_common_final_state(uuid)'::regprocedure
    ]
    LOOP
        SELECT pg_get_functiondef(checked_function) INTO definition;
        updated_definition := replace(
            replace(
                definition,
                'predecessor_terminal_member_count + 1',
                'predecessor_terminal_member_count + turn_lifecycle_origin_member_span(checked_turn_id, checked_session_id)'
            ),
            'predecessor_member_count + 1',
            'predecessor_member_count + turn_lifecycle_origin_member_span(checked_turn_id, checked_session)'
        );
        IF updated_definition = definition THEN
            RAISE EXCEPTION 'delegation wake could not extend lifecycle origin span in %',
                checked_function;
        END IF;
        EXECUTE updated_definition;
    END LOOP;
END;
$migration$;

CREATE FUNCTION require_delegation_wake_turn_origin()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    stored session_delegation_wake_turn_origin%ROWTYPE;
    lifecycle turn_lifecycle%ROWTYPE;
    predecessor_turn uuid;
    predecessor_frontier uuid;
    predecessor_member_count numeric(20, 0);
    predecessor_defaults_version numeric(20, 0);
    predecessor_requested_kind text;
    predecessor_requested_direct uuid;
    predecessor_requested_alias uuid;
    predecessor_frozen_kind text;
    predecessor_frozen_direct uuid;
    predecessor_frozen_alias uuid;
    predecessor_frozen_alias_selected uuid;
    delivery_count bigint;
    incorrect_member_count bigint;
    skipped_earlier_count bigint;
BEGIN
    SELECT * INTO stored FROM session_delegation_wake_turn_origin
     WHERE turn_id = NEW.turn_id;
    IF NOT FOUND THEN
        RETURN NULL;
    END IF;
    SELECT * INTO lifecycle FROM turn_lifecycle
     WHERE turn_id = stored.turn_id
       AND session_id = stored.recipient_session_id
       AND acceptance_position = stored.admission_position
       AND origin_kind = 'delegation';
    IF NOT FOUND THEN
        RAISE EXCEPTION 'delegation wake lacks its exact typed turn lifecycle'
            USING ERRCODE = '23514',
                CONSTRAINT = 'session_delegation_wake_turn_origin';
    END IF;

    SELECT count(*) INTO delivery_count
      FROM generate_series(
            stored.first_delivery_sequence,
            stored.through_delivery_sequence
      ) AS sequence(value)
     WHERE delegation_delivery_semantic_entry(
            stored.recipient_session_id,
            sequence.value
     ) IS NOT NULL;
    IF delivery_count <> stored.through_delivery_sequence
            - stored.first_delivery_sequence + 1 THEN
        RAISE EXCEPTION 'delegation wake range lacks typed delivered content'
            USING ERRCODE = '23514',
                CONSTRAINT = 'session_delegation_wake_turn_origin';
    END IF;
    predecessor_turn := accepted_input_turn_queue_predecessor(
        stored.recipient_session_id,
        stored.turn_id
    );
    SELECT terminal_frontier_id INTO predecessor_frontier
      FROM turn_lifecycle
     WHERE turn_id = predecessor_turn
       AND session_id = stored.recipient_session_id
       AND state_kind = 'terminal';
    SELECT member_count INTO predecessor_member_count
      FROM context_frontier
     WHERE owning_session_id = stored.recipient_session_id
       AND context_frontier_id = predecessor_frontier;
    SELECT
        exact.defaults_version,
        exact.requested_model_kind,
        exact.requested_direct_model_selection_id,
        exact.requested_model_alias_id,
        exact.frozen_model_kind,
        exact.frozen_direct_model_selection_id,
        exact.frozen_model_alias_id,
        exact.frozen_alias_selected_direct_id
      INTO
        predecessor_defaults_version,
        predecessor_requested_kind,
        predecessor_requested_direct,
        predecessor_requested_alias,
        predecessor_frozen_kind,
        predecessor_frozen_direct,
        predecessor_frozen_alias,
        predecessor_frozen_alias_selected
      FROM turn_origin_exact_model_configuration(
            predecessor_turn,
            stored.recipient_session_id
      ) AS exact;
    IF predecessor_member_count IS NULL
       OR stored.defaults_version IS DISTINCT FROM predecessor_defaults_version
       OR stored.requested_model_kind IS DISTINCT FROM predecessor_requested_kind
       OR stored.requested_direct_model_selection_id IS DISTINCT FROM
            predecessor_requested_direct
       OR stored.requested_model_alias_id IS DISTINCT FROM
            predecessor_requested_alias
       OR stored.frozen_model_kind IS DISTINCT FROM predecessor_frozen_kind
       OR stored.frozen_direct_model_selection_id IS DISTINCT FROM
            predecessor_frozen_direct
       OR stored.frozen_model_alias_id IS DISTINCT FROM predecessor_frozen_alias
       OR stored.frozen_alias_selected_direct_id IS DISTINCT FROM
            predecessor_frozen_alias_selected THEN
        RAISE EXCEPTION 'delegation wake lacks its exact terminal predecessor configuration'
            USING ERRCODE = '23514',
                CONSTRAINT = 'session_delegation_wake_turn_origin';
    END IF;
    SELECT count(*) INTO skipped_earlier_count
      FROM session_pending_delivery AS pending
     WHERE pending.recipient_session_id = stored.recipient_session_id
       AND pending.delivery_sequence < stored.first_delivery_sequence
       AND NOT EXISTS (
            SELECT 1 FROM context_frontier_member AS member
             WHERE member.owning_session_id = stored.recipient_session_id
               AND member.context_frontier_id = predecessor_frontier
               AND member.source_session_id = stored.recipient_session_id
               AND member.semantic_entry_id = delegation_delivery_semantic_entry(
                    stored.recipient_session_id,
                    pending.delivery_sequence
               )
       );
    IF skipped_earlier_count <> 0 THEN
        RAISE EXCEPTION 'delegation wake starting frontier skips earlier delivery content'
            USING ERRCODE = '23514',
                CONSTRAINT = 'session_delegation_wake_turn_origin';
    END IF;
    IF lifecycle.state_kind = 'queued' THEN
        RETURN NULL;
    END IF;

    IF lifecycle.start_lineage_kind <> 'after'
       OR lifecycle.immediate_predecessor_turn_id IS DISTINCT FROM predecessor_turn
    THEN
        RAISE EXCEPTION 'delegation wake lacks its exact terminal predecessor'
            USING ERRCODE = '23514',
                CONSTRAINT = 'session_delegation_wake_turn_origin';
    END IF;

    SELECT count(*) INTO incorrect_member_count
      FROM generate_series(
            stored.first_delivery_sequence,
            stored.through_delivery_sequence
      ) AS sequence(value)
      LEFT JOIN context_frontier_member AS member
        ON member.owning_session_id = stored.recipient_session_id
       AND member.context_frontier_id = lifecycle.starting_frontier_id
       AND member.member_position = predecessor_member_count
            + sequence.value - stored.first_delivery_sequence + 1
            + CASE
                WHEN sequence.value = stored.through_delivery_sequence THEN
                    turn_start_model_identity_entry_count(
                        stored.turn_id,
                        lifecycle.starting_frontier_id
                    )
                ELSE 0
              END
       AND member.source_session_id = stored.recipient_session_id
       AND member.semantic_entry_id = delegation_delivery_semantic_entry(
            stored.recipient_session_id,
            sequence.value
       )
     WHERE member.semantic_entry_id IS NULL;
    IF incorrect_member_count <> 0 THEN
        RAISE EXCEPTION 'delegation wake starting frontier skips or reorders delivery content'
            USING ERRCODE = '23514',
                CONSTRAINT = 'session_delegation_wake_turn_origin';
    END IF;
    RETURN NULL;
END;
$$;
CREATE CONSTRAINT TRIGGER session_delegation_wake_turn_origin_is_valid
AFTER INSERT OR UPDATE ON session_delegation_wake_turn_origin
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION require_delegation_wake_turn_origin();
CREATE CONSTRAINT TRIGGER turn_lifecycle_requires_valid_delegation_wake_origin
AFTER INSERT OR UPDATE ON turn_lifecycle
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW WHEN (NEW.origin_kind = 'delegation')
EXECUTE FUNCTION require_delegation_wake_turn_origin();

-- Foreground delivery completes the exact await_session tool request. A
-- background wait already completed with its registration receipt, so its
-- later wake content retains the await identity only as delegation provenance
-- and must not become a second logical tool result.
CREATE FUNCTION require_semantic_delegation_result_delivery_mode()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM session_child_result_delivery AS delivery
         WHERE delivery.awaiting_tool_request_id =
                NEW.delegation_result_awaiting_tool_request_id
           AND delivery.spawning_tool_request_id =
                NEW.delegation_result_spawning_tool_request_id
           AND delivery.parent_session_id = NEW.source_session_id
           AND (
                (
                    delivery.delivery_sequence IS NULL
                    AND NEW.tool_result_request_id =
                        NEW.delegation_result_awaiting_tool_request_id
                )
                OR (
                    delivery.delivery_sequence IS NOT NULL
                    AND NEW.tool_result_request_id IS NULL
                )
           )
    ) THEN
        RAISE EXCEPTION 'delegation result correlation contradicts its delivery mode'
            USING ERRCODE = '23514',
                CONSTRAINT = 'semantic_delegation_result_delivery_mode';
    END IF;
    RETURN NULL;
END;
$$;
CREATE CONSTRAINT TRIGGER semantic_delegation_result_delivery_mode
AFTER INSERT ON semantic_transcript_entry
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW WHEN (NEW.payload_kind = 'delegation_result')
EXECUTE FUNCTION require_semantic_delegation_result_delivery_mode();

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

-- Delegation owns a distinct header family so a later migration extending the
-- baseline outbox kind inventory cannot rewrite this migration's contract.
-- Both header families use the same allocator and delivery prefix.
CREATE TABLE delegation_outbox_event (
    event_sequence numeric(20, 0) PRIMARY KEY,
    event_kind text NOT NULL CHECK (
        event_kind IN ('delegation_update', 'delegation_wake')
    ),
    storage_version smallint NOT NULL CHECK (storage_version = 1),
    session_id uuid NOT NULL,
    UNIQUE (event_sequence, event_kind, storage_version, session_id),
    FOREIGN KEY (session_id) REFERENCES session(session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED
);
CREATE INDEX delegation_outbox_event_by_session_sequence
    ON delegation_outbox_event(session_id, event_sequence);
CREATE TRIGGER delegation_outbox_event_allocates_sequence
BEFORE INSERT ON delegation_outbox_event
FOR EACH ROW EXECUTE FUNCTION allocate_outbox_event_sequence();
CREATE TRIGGER delegation_outbox_event_is_append_only
BEFORE UPDATE OR DELETE ON delegation_outbox_event
FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();
CREATE TRIGGER delegation_outbox_event_cannot_be_truncated
BEFORE TRUNCATE ON delegation_outbox_event
FOR EACH STATEMENT EXECUTE FUNCTION reject_outbox_table_truncate();

CREATE OR REPLACE FUNCTION require_outbox_sequence_event()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.last_sequence <> OLD.last_sequence + 1 THEN
        RAISE EXCEPTION 'outbox sequence must advance exactly once per event'
            USING ERRCODE = '23514';
    END IF;
    IF NEW.last_allocation_xid IS DISTINCT FROM pg_current_xact_id() THEN
        RAISE EXCEPTION 'outbox allocator transaction must accompany its sequence'
            USING ERRCODE = '23514';
    END IF;
    IF NOT EXISTS (
        SELECT 1 FROM outbox_event
         WHERE event_sequence = NEW.last_sequence
    ) AND NOT EXISTS (
        SELECT 1 FROM delegation_outbox_event
         WHERE event_sequence = NEW.last_sequence
    ) THEN
        RAISE EXCEPTION 'outbox sequence % requires its event row', NEW.last_sequence
            USING ERRCODE = '23503';
    END IF;
    RETURN NULL;
END;
$$;

CREATE OR REPLACE FUNCTION require_next_outbox_delivery()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.singleton <> OLD.singleton
        OR NEW.delivered_through <> OLD.delivered_through + 1 THEN
        RAISE EXCEPTION 'outbox delivery must advance by exactly one sequence'
            USING ERRCODE = '23514';
    END IF;
    IF EXISTS (
        SELECT 1 FROM outbox_sequence_state
         WHERE singleton AND last_allocation_xid = pg_current_xact_id()
    ) THEN
        RAISE EXCEPTION
            'outbox delivery cannot advance in an event-producing transaction'
            USING ERRCODE = '23514';
    END IF;
    NEW.last_delivery_xid := pg_current_xact_id();
    IF NOT EXISTS (
        SELECT 1 FROM outbox_event
         WHERE event_sequence = NEW.delivered_through
    ) AND NOT EXISTS (
        SELECT 1 FROM delegation_outbox_event
         WHERE event_sequence = NEW.delivered_through
    ) THEN
        RAISE EXCEPTION 'outbox delivery sequence % requires a committed event',
            NEW.delivered_through USING ERRCODE = '23503';
    END IF;
    RETURN NEW;
END;
$$;

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
            AND outcome_kind IN (
                'child_stopped', 'child_cancelled', 'already_terminal', 'continue_running'
            )
            AND reason_kind IN (
                'parent_stopped_parent_and_descendants',
                'parent_cancelled_parent_and_descendants'
            )
            AND provenance_kind IN ('parent_turn_command', 'parent_goal_command')
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
        REFERENCES delegation_outbox_event(
            event_sequence, event_kind, storage_version, session_id
        )
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
                       AND event.outcome_kind IN (
                            'child_stopped', 'child_cancelled',
                            'already_terminal', 'continue_running'
                       )
                       AND event.reason_kind IN (
                            'parent_stopped_parent_and_descendants',
                            'parent_cancelled_parent_and_descendants'
                       )
                       AND event.provenance_kind IN (
                            'parent_turn_command', 'parent_goal_command'
                       )
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
DECLARE
    waiting_update_sequence numeric(20, 0);
    waiting_update_count bigint;
BEGIN
    SELECT count(*), min(event_sequence)
      INTO waiting_update_count, waiting_update_sequence
      FROM delegation_update_outbox_event
     WHERE update_kind = 'child_waiting'
       AND awaiting_tool_request_id = NEW.awaiting_tool_request_id
       AND session_id = NEW.parent_session_id;
    IF waiting_update_count <> 1 THEN
        RAISE EXCEPTION 'delegation wait requires exactly one child-waiting update'
            USING ERRCODE = '23503',
                CONSTRAINT = 'delegation_child_waiting_update_required';
    END IF;
    IF NEW.wait_mode = 'foreground'
        AND EXISTS (
            SELECT 1 FROM session_child_result
             WHERE spawning_tool_request_id = NEW.spawning_tool_request_id
        )
        AND NOT EXISTS (
            SELECT 1 FROM delegation_wake_outbox_event AS wake
             WHERE wake.subject_kind = 'result'
               AND wake.result_spawning_request_id =
                    NEW.spawning_tool_request_id
               AND wake.session_id = NEW.parent_session_id
               AND wake.event_sequence > waiting_update_sequence
               AND (
                    wake.awaiting_tool_request_id IS NULL
                    OR wake.awaiting_tool_request_id =
                        NEW.awaiting_tool_request_id
               )
        )
    THEN
        RAISE EXCEPTION 'late foreground wait requires a fresh durable result wake'
            USING ERRCODE = '23503',
                CONSTRAINT = 'delegation_late_wait_wake_required';
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
    IF NEW.event_kind = 'outcome_recorded'
        AND NEW.provenance_kind IN (
            'parent_turn_command', 'parent_goal_command'
        ) AND (
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
    awaiting_tool_request_id uuid,
    message_id uuid,
    CONSTRAINT delegation_wake_subject_shape CHECK ((subject_kind = 'result'
            AND result_spawning_request_id IS NOT NULL
            AND result_spawning_request_id = spawning_tool_request_id
            AND message_id IS NULL)
        OR (subject_kind = 'message' AND result_spawning_request_id IS NULL
            AND awaiting_tool_request_id IS NULL AND message_id IS NOT NULL)),
    FOREIGN KEY (event_sequence, event_kind, storage_version, session_id)
        REFERENCES delegation_outbox_event(
            event_sequence, event_kind, storage_version, session_id
        )
        ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (result_spawning_request_id)
        REFERENCES session_child_result(spawning_tool_request_id)
        ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (result_spawning_request_id, session_id)
        REFERENCES session_delegation(spawning_tool_request_id, parent_session_id)
        ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (
        awaiting_tool_request_id, result_spawning_request_id, session_id
    ) REFERENCES session_delegation_wait(
        awaiting_tool_request_id, spawning_tool_request_id, parent_session_id
    ) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (message_id, spawning_tool_request_id)
        REFERENCES session_message(message_id, spawning_tool_request_id)
        ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED
);
CREATE UNIQUE INDEX delegation_initial_result_wake_once
    ON delegation_wake_outbox_event(result_spawning_request_id)
    WHERE subject_kind = 'result' AND awaiting_tool_request_id IS NULL;
CREATE UNIQUE INDEX delegation_late_wait_wake_once
    ON delegation_wake_outbox_event(awaiting_tool_request_id)
    WHERE subject_kind = 'result' AND awaiting_tool_request_id IS NOT NULL;
CREATE UNIQUE INDEX delegation_message_wake_once
    ON delegation_wake_outbox_event(message_id)
    WHERE subject_kind = 'message';
CREATE FUNCTION require_delegation_wake_recipient()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF (NEW.subject_kind = 'result' AND (
            NOT EXISTS (
                SELECT 1 FROM session_delegation
                 WHERE spawning_tool_request_id = NEW.spawning_tool_request_id
                   AND parent_session_id = NEW.session_id
            )
            OR (NEW.awaiting_tool_request_id IS NOT NULL AND NOT EXISTS (
                SELECT 1 FROM session_delegation_wait
                 WHERE awaiting_tool_request_id = NEW.awaiting_tool_request_id
                   AND spawning_tool_request_id = NEW.spawning_tool_request_id
                   AND parent_session_id = NEW.session_id
            ))))
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
           AND wake.awaiting_tool_request_id IS NULL
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
CREATE FUNCTION require_delegation_outbox_event_typed_record()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE matching_records bigint;
BEGIN
    CASE NEW.event_kind
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
CREATE CONSTRAINT TRIGGER delegation_outbox_event_requires_typed_record
AFTER INSERT ON delegation_outbox_event DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION require_delegation_outbox_event_typed_record();

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
CREATE TRIGGER session_delegation_wake_turn_origin_cannot_be_truncated
BEFORE TRUNCATE ON session_delegation_wake_turn_origin
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
