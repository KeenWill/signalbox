-- Delegation: parent/child session relations and their event log, waits and
-- wake origins, termination cascades, inter-session messages and child
-- results with their deliveries, the pending-delivery queue, and the
-- delegation-scoped outbox events.

-- Function bodies may read tables a later section or file creates;
-- 202609010000_core.sql explains why validation is deferred.
SET check_function_bodies = false;

--
-- Functions.
--

--
-- Name: delegation_cascade_expected_frontier(uuid, text); Type: FUNCTION; Schema: public
--

CREATE FUNCTION delegation_cascade_expected_frontier(checked_root_session uuid, checked_root_kind text) RETURNS TABLE(spawning_tool_request_id uuid, parent_session_id uuid, child_session_id uuid, effective_parent_kind text, source_kind text, source_spawning_tool_request_id uuid, expected_action text)
    LANGUAGE plpgsql STABLE
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
                WHEN parent_result.outcome_kind = 'child_stopped' THEN 'stopped'
                WHEN parent_result.outcome_kind = 'child_cancelled' THEN 'cancelled'
                WHEN parent_result.spawning_tool_request_id IS NOT NULL THEN
                    parent.effective_parent_kind
                WHEN parent.expected_action = 'stop' THEN 'stopped'
                WHEN parent.expected_action = 'cancel' THEN 'cancelled'
            END AS effective_parent_kind,
            'parent_disposition'::text AS source_kind,
            parent.spawning_tool_request_id AS source_spawning_tool_request_id,
            CASE
                WHEN relation.policy_kind = 'background' THEN 'keep_running'
                WHEN parent_result.outcome_kind = 'child_stopped'
                    THEN relation.on_parent_stopped
                WHEN parent_result.outcome_kind = 'child_cancelled'
                    THEN relation.on_parent_cancelled
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


--
-- Name: delegation_delivery_semantic_entry(uuid, numeric); Type: FUNCTION; Schema: public
--

CREATE FUNCTION delegation_delivery_semantic_entry(checked_recipient uuid, checked_sequence numeric) RETURNS uuid
    LANGUAGE sql STABLE
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


--
-- Name: guard_session_delegation_event_append(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION guard_session_delegation_event_append() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
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


--
-- Name: guard_session_delegation_wake_turn_origin_change(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION guard_session_delegation_wake_turn_origin_change() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
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


--
-- Name: guard_session_pending_delivery_append(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION guard_session_pending_delivery_append() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
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


--
-- Name: lock_delegation_frontier_before_goal_stop(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION lock_delegation_frontier_before_goal_stop() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
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


--
-- Name: lock_delegation_frontier_before_input_interrupt(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION lock_delegation_frontier_before_input_interrupt() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
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


--
-- Name: lock_delegation_parent_for_spawn(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION lock_delegation_parent_for_spawn() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    PERFORM 1 FROM session
     WHERE session_id = NEW.parent_session_id
     FOR NO KEY UPDATE;
    RETURN NEW;
END;
$$;


--
-- Name: lock_delegation_termination_frontier(uuid, text); Type: FUNCTION; Schema: public
--

CREATE FUNCTION lock_delegation_termination_frontier(checked_root_session uuid, checked_root_kind text) RETURNS void
    LANGUAGE plpgsql
    AS $$
BEGIN
    PERFORM lock_delegation_termination_session_frontier(
        checked_root_session,
        checked_root_kind
    );
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


--
-- Name: lock_delegation_termination_session_frontier(uuid, text); Type: FUNCTION; Schema: public
--

CREATE FUNCTION lock_delegation_termination_session_frontier(checked_root_session uuid, checked_root_kind text) RETURNS void
    LANGUAGE plpgsql
    AS $$
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
            SELECT relation.parent_session_id
              FROM session_delegation AS relation
             WHERE relation.child_session_id = checked_root_session
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
END;
$$;


--
-- Name: materialize_session_delegation_termination_cascade(uuid); Type: FUNCTION; Schema: public
--

CREATE FUNCTION materialize_session_delegation_termination_cascade(checked_root_command uuid) RETURNS void
    LANGUAGE plpgsql
    AS $$
DECLARE
    root_session uuid;
    root_source_kind text;
    root_turn uuid;
    root_goal_generation numeric(20, 0);
    root_termination_kind text;
    provenance_kind text;
    disposition_count numeric(20, 0);
    frontier record;
    parent_turn uuid;
    event_ordinal numeric(20, 0);
    outcome_kind text;
    reason_kind text;
    logical_disposition text;
    child_turn uuid;
    child_starting_frontier uuid;
    child_terminal_frontier uuid;
    child_frontier_member_count numeric(20, 0);
    child_task_entry uuid;
    wait_record record;
    delivery_sequence numeric(20, 0);
BEGIN
    SELECT
        root.root_session_id,
        root.root_source_kind,
        root.root_turn_id,
        root.root_goal_generation,
        root.termination_kind
      INTO
        root_session,
        root_source_kind,
        root_turn,
        root_goal_generation,
        root_termination_kind
      FROM (
        SELECT
            command.session_id AS root_session_id,
            'goal_command'::text AS root_source_kind,
            NULL::uuid AS root_turn_id,
            event.generation AS root_goal_generation,
            'stopped'::text AS termination_kind
          FROM goal_command AS command
          JOIN goal_event AS event
            ON event.session_id = command.session_id
           AND event.event_ordinal = command.result_event_ordinal
         WHERE command.command_id = checked_root_command
           AND command.operation_kind = 'stop'
           AND command.result_kind = 'applied'
           AND command.descendant_scope = 'parent_and_descendants'
           AND event.event_kind = 'user_stopped'
        UNION ALL
        SELECT
            command.session_id,
            'turn_command'::text,
            command.expected_active_turn_id,
            NULL::numeric(20, 0),
            'cancelled'::text
          FROM submit_input_command AS command
         WHERE command.command_id = checked_root_command
           AND command.delivery_kind = 'interrupt'
           AND command.result_kind = 'applied'
           AND command.descendant_scope = 'parent_and_descendants'
      ) AS root;
    IF root_session IS NULL THEN
        RETURN;
    END IF;

    SELECT count(*) INTO disposition_count
      FROM delegation_cascade_expected_frontier(
            root_session, root_termination_kind
      );
    IF disposition_count = 0 THEN
        RETURN;
    END IF;
    provenance_kind := CASE root_source_kind
        WHEN 'goal_command' THEN 'parent_goal_command'
        WHEN 'turn_command' THEN 'parent_turn_command'
    END;

    INSERT INTO session_delegation_termination_cascade
        (root_command_id, root_session_id, root_source_kind,
         root_turn_id, root_goal_generation, termination_kind,
         descendant_scope, disposition_count)
    VALUES
        (checked_root_command, root_session, root_source_kind,
         root_turn, root_goal_generation, root_termination_kind,
         'parent_and_descendants', disposition_count);

    FOR frontier IN
        SELECT expected.*
          FROM delegation_cascade_expected_frontier(
                root_session, root_termination_kind
          ) AS expected
         ORDER BY expected.parent_session_id,
                  expected.spawning_tool_request_id
    LOOP
        parent_turn := NULL;
        IF root_source_kind = 'turn_command' THEN
            IF frontier.source_kind = 'root' THEN
                parent_turn := root_turn;
            ELSE
                SELECT task.turn_id INTO parent_turn
                  FROM session_delegation_initial_task AS task
                 WHERE task.spawning_tool_request_id =
                        frontier.source_spawning_tool_request_id;
            END IF;
        END IF;

        INSERT INTO session_delegation_parent_termination
            (spawning_tool_request_id, root_command_id, parent_session_id,
             command_source_kind, parent_turn_id, parent_goal_generation,
             termination_kind, source_kind,
             source_spawning_tool_request_id)
        VALUES
            (frontier.spawning_tool_request_id, checked_root_command,
             frontier.parent_session_id, root_source_kind, parent_turn,
             root_goal_generation, frontier.effective_parent_kind,
             frontier.source_kind, frontier.source_spawning_tool_request_id);

        IF EXISTS (
            SELECT 1 FROM session_child_result AS result
             WHERE result.spawning_tool_request_id =
                    frontier.spawning_tool_request_id
        ) THEN
            outcome_kind := 'already_terminal';
            logical_disposition := NULL;
        ELSE
            outcome_kind := CASE frontier.expected_action
                WHEN 'keep_running' THEN 'continue_running'
                WHEN 'stop' THEN 'child_stopped'
                WHEN 'cancel' THEN 'child_cancelled'
            END;
            logical_disposition := CASE frontier.expected_action
                WHEN 'stop' THEN 'stopped'
                WHEN 'cancel' THEN 'cancelled'
            END;
        END IF;
        reason_kind := CASE frontier.effective_parent_kind
            WHEN 'stopped' THEN 'parent_stopped_parent_and_descendants'
            WHEN 'cancelled' THEN 'parent_cancelled_parent_and_descendants'
        END;

        IF logical_disposition IS NOT NULL THEN
            SELECT task.turn_id, task.semantic_entry_id,
                   lifecycle.starting_frontier_id
              INTO child_turn, child_task_entry, child_starting_frontier
              FROM session_delegation_initial_task AS task
              JOIN turn_lifecycle AS lifecycle
                ON lifecycle.turn_id = task.turn_id
               AND lifecycle.session_id = task.child_session_id
             WHERE task.spawning_tool_request_id =
                    frontier.spawning_tool_request_id
               AND task.child_session_id = frontier.child_session_id;
            child_terminal_frontier := (
                md5(
                    'signalbox:delegation-terminal-frontier:'
                    || frontier.spawning_tool_request_id::text
                )
            )::uuid;
            IF child_starting_frontier IS NULL THEN
                INSERT INTO context_frontier
                    (owning_session_id, context_frontier_id, member_count,
                     prefix_context_frontier_id)
                VALUES
                    (frontier.child_session_id, child_terminal_frontier,
                     1, NULL);
                INSERT INTO context_frontier_delta
                    (owning_session_id, context_frontier_id, member_position,
                     source_session_id, semantic_entry_id)
                VALUES
                    (frontier.child_session_id, child_terminal_frontier, 1,
                     frontier.child_session_id, child_task_entry);
            ELSE
                SELECT stored.member_count INTO child_frontier_member_count
                  FROM context_frontier AS stored
                 WHERE stored.owning_session_id = frontier.child_session_id
                   AND stored.context_frontier_id = child_starting_frontier;
                INSERT INTO context_frontier
                    (owning_session_id, context_frontier_id, member_count,
                     prefix_context_frontier_id)
                VALUES
                    (frontier.child_session_id, child_terminal_frontier,
                     child_frontier_member_count, child_starting_frontier);
            END IF;
            INSERT INTO session_delegation_logical_terminal
                (spawning_tool_request_id, child_session_id, child_turn_id,
                 root_command_id, terminal_frontier_id, disposition_kind)
            VALUES
                (frontier.spawning_tool_request_id, frontier.child_session_id,
                 child_turn, checked_root_command, child_terminal_frontier,
                 logical_disposition);
            UPDATE turn_lifecycle
               SET delegation_runtime_terminal = true
             WHERE session_id = frontier.child_session_id
               AND turn_id = child_turn;
        END IF;

        SELECT COALESCE(max(stored.event_ordinal), 0) + 1
          INTO event_ordinal
          FROM session_delegation_event AS stored
         WHERE stored.spawning_tool_request_id =
                frontier.spawning_tool_request_id;
        INSERT INTO session_delegation_event
            (spawning_tool_request_id, event_ordinal, event_kind,
             outcome_kind, reason_kind, provenance_kind,
             provenance_session_id, provenance_turn_id,
             provenance_goal_generation, provenance_command_id)
        VALUES
            (frontier.spawning_tool_request_id, event_ordinal,
             'outcome_recorded', outcome_kind, reason_kind, provenance_kind,
             frontier.parent_session_id, parent_turn, root_goal_generation,
             checked_root_command);

        WITH header AS (
            INSERT INTO delegation_outbox_event
                (event_kind, storage_version, session_id)
            VALUES ('delegation_update', 1, frontier.parent_session_id)
            RETURNING event_sequence, event_kind, storage_version, session_id
        )
        INSERT INTO delegation_update_outbox_event
            (event_sequence, event_kind, storage_version, session_id,
             update_kind, spawning_tool_request_id, child_session_id,
             delegation_event_ordinal, delegation_event_kind,
             outcome_kind, reason_kind, provenance_kind,
             provenance_session_id, provenance_turn_id,
             provenance_goal_generation, provenance_command_id)
        SELECT
            event_sequence, event_kind, storage_version, session_id,
            'child_lifecycle_disposition',
            frontier.spawning_tool_request_id, frontier.child_session_id,
            event_ordinal, 'outcome_recorded', outcome_kind, reason_kind,
            provenance_kind, frontier.parent_session_id, parent_turn,
            root_goal_generation, checked_root_command
          FROM header;

        IF logical_disposition IS NOT NULL THEN
            WITH header AS (
                INSERT INTO delegation_outbox_event
                    (event_kind, storage_version, session_id)
                VALUES ('delegation_update', 1, frontier.child_session_id)
                RETURNING event_sequence, event_kind, storage_version, session_id
            )
            INSERT INTO delegation_update_outbox_event
                (event_sequence, event_kind, storage_version, session_id,
                 update_kind, spawning_tool_request_id, child_session_id,
                 delegation_event_ordinal, delegation_event_kind,
                 outcome_kind, reason_kind, provenance_kind,
                 provenance_session_id, provenance_turn_id,
                 provenance_goal_generation, provenance_command_id)
            SELECT
                event_sequence, event_kind, storage_version, session_id,
                'child_lifecycle_disposition',
                frontier.spawning_tool_request_id, frontier.child_session_id,
                event_ordinal, 'outcome_recorded', outcome_kind, reason_kind,
                provenance_kind, frontier.parent_session_id, parent_turn,
                root_goal_generation, checked_root_command
              FROM header;

            INSERT INTO session_child_result
                (spawning_tool_request_id, event_ordinal, event_kind,
                 outcome_kind)
            VALUES
                (frontier.spawning_tool_request_id, event_ordinal,
                 'outcome_recorded', outcome_kind);

            FOR wait_record IN
                SELECT waiting.awaiting_tool_request_id, waiting.wait_mode
                  FROM session_delegation_wait AS waiting
                 WHERE waiting.spawning_tool_request_id =
                        frontier.spawning_tool_request_id
                 ORDER BY waiting.awaiting_tool_request_id
            LOOP
                delivery_sequence := NULL;
                IF wait_record.wait_mode = 'background' THEN
                    SELECT COALESCE(max(pending.delivery_sequence), 0) + 1
                      INTO delivery_sequence
                      FROM session_pending_delivery AS pending
                     WHERE pending.recipient_session_id =
                            frontier.parent_session_id;
                    INSERT INTO session_pending_delivery
                        (recipient_session_id, delivery_sequence, delivery_kind)
                    VALUES
                        (frontier.parent_session_id, delivery_sequence,
                         'background_result');
                END IF;
                INSERT INTO session_child_result_delivery
                    (awaiting_tool_request_id, spawning_tool_request_id,
                     parent_session_id, delivery_sequence, delivery_kind)
                VALUES
                    (wait_record.awaiting_tool_request_id,
                     frontier.spawning_tool_request_id,
                     frontier.parent_session_id, delivery_sequence,
                     CASE WHEN delivery_sequence IS NULL THEN NULL
                          ELSE 'background_result' END);
            END LOOP;

            WITH header AS (
                INSERT INTO delegation_outbox_event
                    (event_kind, storage_version, session_id)
                VALUES ('delegation_update', 1, frontier.parent_session_id)
                RETURNING event_sequence, event_kind, storage_version, session_id
            )
            INSERT INTO delegation_update_outbox_event
                (event_sequence, event_kind, storage_version, session_id,
                 update_kind, spawning_tool_request_id, child_session_id,
                 outcome_kind, reason_kind, provenance_kind,
                 provenance_session_id, provenance_turn_id,
                 provenance_goal_generation, provenance_command_id,
                 result_spawning_request_id)
            SELECT
                event_sequence, event_kind, storage_version, session_id,
                'child_result', frontier.spawning_tool_request_id,
                frontier.child_session_id, outcome_kind, reason_kind,
                provenance_kind, frontier.parent_session_id, parent_turn,
                root_goal_generation, checked_root_command,
                frontier.spawning_tool_request_id
              FROM header;

            WITH header AS (
                INSERT INTO delegation_outbox_event
                    (event_kind, storage_version, session_id)
                VALUES ('delegation_wake', 1, frontier.parent_session_id)
                RETURNING event_sequence, event_kind, storage_version, session_id
            )
            INSERT INTO delegation_wake_outbox_event
                (event_sequence, event_kind, storage_version, session_id,
                 spawning_tool_request_id, subject_kind,
                 result_spawning_request_id, awaiting_tool_request_id)
            SELECT
                event_sequence, event_kind, storage_version, session_id,
                frontier.spawning_tool_request_id, 'result',
                frontier.spawning_tool_request_id, NULL
              FROM header;
        END IF;
    END LOOP;
END;
$$;


--
-- Name: reject_session_delegation_table_truncate(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION reject_session_delegation_table_truncate() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    RAISE EXCEPTION 'delegation relations and histories are append-only'
        USING ERRCODE = '23514';
END;
$$;


--
-- Name: require_applied_goal_command_delegation_cascade(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_applied_goal_command_delegation_cascade() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
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


--
-- Name: require_applied_turn_command_delegation_cascade(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_applied_turn_command_delegation_cascade() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
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


--
-- Name: require_delegated_session_credential_purpose(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_delegated_session_credential_purpose() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
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


--
-- Name: require_delegation_cascade_disposition_count(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_delegation_cascade_disposition_count() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
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


--
-- Name: require_delegation_cascade_parent_termination_chains(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_delegation_cascade_parent_termination_chains() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
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


--
-- Name: require_delegation_initial_task_origin(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_delegation_initial_task_origin() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
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


--
-- Name: require_delegation_initial_task_purpose(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_delegation_initial_task_purpose() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
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


--
-- Name: require_delegation_lifecycle_update(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_delegation_lifecycle_update() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NEW.event_kind <> 'outcome_recorded'
        OR NEW.provenance_kind NOT IN (
            'parent_turn_command', 'parent_goal_command'
        ) THEN
        RETURN NULL;
    END IF;
    IF (
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
    IF NEW.outcome_kind IN ('child_stopped', 'child_cancelled') AND (
        SELECT count(*) FROM delegation_update_outbox_event AS emitted
          JOIN session_delegation AS relation
            ON relation.spawning_tool_request_id = NEW.spawning_tool_request_id
         WHERE emitted.update_kind = 'child_lifecycle_disposition'
           AND emitted.spawning_tool_request_id = NEW.spawning_tool_request_id
           AND emitted.delegation_event_ordinal = NEW.event_ordinal
           AND emitted.session_id = relation.child_session_id
    ) <> 1 THEN
        RAISE EXCEPTION
            'terminalized delegated child requires exactly one lifecycle update'
            USING ERRCODE = '23503',
                CONSTRAINT = 'delegation_child_lifecycle_update_required';
    END IF;
    RETURN NULL;
END;
$$;


--
-- Name: require_delegation_logical_terminal_outcome(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_delegation_logical_terminal_outcome() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM session_delegation_event AS event
          JOIN session_child_result AS result
            ON result.spawning_tool_request_id = event.spawning_tool_request_id
           AND result.event_ordinal = event.event_ordinal
           AND result.event_kind = event.event_kind
           AND result.outcome_kind = event.outcome_kind
         WHERE event.spawning_tool_request_id = NEW.spawning_tool_request_id
           AND event.event_kind = 'outcome_recorded'
           AND event.provenance_command_id = NEW.root_command_id
           AND event.outcome_kind = CASE NEW.disposition_kind
                WHEN 'stopped' THEN 'child_stopped'
                WHEN 'cancelled' THEN 'child_cancelled'
           END
    ) THEN
        RAISE EXCEPTION 'logical delegation terminal lacks its exact delivered outcome'
            USING ERRCODE = '23503',
                CONSTRAINT = 'session_delegation_logical_terminal_outcome';
    END IF;
    IF NOT EXISTS (
        SELECT 1 FROM turn_lifecycle AS lifecycle
         WHERE lifecycle.session_id = NEW.child_session_id
           AND lifecycle.turn_id = NEW.child_turn_id
           AND lifecycle.delegation_runtime_terminal
    ) THEN
        RAISE EXCEPTION 'logical delegation terminal did not release its runtime slot'
            USING ERRCODE = '23503',
                CONSTRAINT = 'session_delegation_logical_terminal_runtime_slot';
    END IF;
    RETURN NULL;
END;
$$;


--
-- Name: require_delegation_message_rejection_attempt(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_delegation_message_rejection_attempt() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF (SELECT count(*) FROM tool_attempt
         WHERE request_id = NEW.tool_request_id
           AND state_kind = 'terminal'
           AND terminal_disposition_kind = 'known_failed'
           AND error_kind = 'execution_failed') <> 1 THEN
        RAISE EXCEPTION 'delegation message rejection lacks its terminal attempt'
            USING ERRCODE = '23503',
                CONSTRAINT = 'session_delegation_message_rejection_attempt';
    END IF;
    RETURN NULL;
END;
$$;


--
-- Name: require_delegation_message_update(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_delegation_message_update() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
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


--
-- Name: require_delegation_message_wake(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_delegation_message_wake() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
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


--
-- Name: require_delegation_outbox_event_typed_record(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_delegation_outbox_event_typed_record() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
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


--
-- Name: require_delegation_parent_termination_chain(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_delegation_parent_termination_chain() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
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


--
-- Name: require_delegation_result_update(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_delegation_result_update() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
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


--
-- Name: require_delegation_result_wake(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_delegation_result_wake() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
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


--
-- Name: require_delegation_spawn_history(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_delegation_spawn_history() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
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


--
-- Name: require_delegation_spawn_update(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_delegation_spawn_update() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
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


--
-- Name: require_delegation_termination_cascade_command(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_delegation_termination_cascade_command() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
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


--
-- Name: require_delegation_update_subject(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_delegation_update_subject() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
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
                       AND (
                            NEW.session_id = relation.parent_session_id
                            OR (
                                NEW.session_id = relation.child_session_id
                                AND event.outcome_kind IN (
                                    'child_stopped', 'child_cancelled'
                                )
                            )
                       )
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


--
-- Name: require_delegation_wait_purpose(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_delegation_wait_purpose() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
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


--
-- Name: require_delegation_wait_rejection_attempt(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_delegation_wait_rejection_attempt() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF (SELECT count(*) FROM tool_attempt
         WHERE request_id = NEW.tool_request_id
           AND state_kind = 'terminal'
           AND terminal_disposition_kind = 'known_failed'
           AND error_kind = 'execution_failed') <> 1 THEN
        RAISE EXCEPTION 'delegation wait rejection lacks its terminal attempt'
            USING ERRCODE = '23503',
                CONSTRAINT = 'session_delegation_wait_rejection_attempt';
    END IF;
    RETURN NULL;
END;
$$;


--
-- Name: require_delegation_wait_turn_phase(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_delegation_wait_turn_phase() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
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


--
-- Name: require_delegation_wait_update(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_delegation_wait_update() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
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


--
-- Name: require_delegation_wake_recipient(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_delegation_wake_recipient() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
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


--
-- Name: require_delegation_wake_turn_origin(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_delegation_wake_turn_origin() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
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
    SELECT turn_lifecycle_effective_terminal_frontier(
                stored.recipient_session_id, predecessor_turn
           )
      INTO predecessor_frontier;
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


--
-- Name: require_initial_task_relation_history(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_initial_task_relation_history() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
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


--
-- Name: require_semantic_delegation_result_delivery_mode(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_semantic_delegation_result_delivery_mode() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
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


--
-- Name: require_session_child_result_delivery_mode(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_session_child_result_delivery_mode() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
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


--
-- Name: require_session_child_result_wait_deliveries(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_session_child_result_wait_deliveries() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
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


--
-- Name: require_session_delegation_event_payload(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_session_delegation_event_payload() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
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
                       AND (
                            (
                            lifecycle.terminal_disposition_kind = 'reconciliation_required'
                            AND EXISTS (
                                SELECT 1
                                  FROM automatic_reconciliation AS recovery
                                 WHERE recovery.turn_id = lifecycle.turn_id
                                   AND recovery.session_id = lifecycle.session_id
                                   AND recovery.model_call_id = lifecycle.terminal_model_call_id
                                   AND recovery.state_kind = 'reconciled'
                            )
                            )
                            OR (
                                lifecycle.terminal_disposition_kind = 'completed'
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
                            )
                       )
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
                )
                AND NOT EXISTS (
                    SELECT 1
                      FROM session_delegation_logical_terminal AS terminal
                     WHERE terminal.spawning_tool_request_id =
                            NEW.spawning_tool_request_id
                       AND terminal.child_session_id = relation_child
                       AND terminal.root_command_id = NEW.provenance_command_id
                       AND terminal.disposition_kind = CASE NEW.outcome_kind
                            WHEN 'child_stopped' THEN 'stopped'
                            WHEN 'child_cancelled' THEN 'cancelled'
                       END
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


--
-- Name: require_session_message_delivery(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_session_message_delivery() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
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


--
-- Name: require_session_message_delivery_recipient(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_session_message_delivery_recipient() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
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


--
-- Name: require_session_pending_delivery_satellite(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_session_pending_delivery_satellite() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
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


--
-- Name: require_terminal_delegated_turn_result(); Type: FUNCTION; Schema: public
--

CREATE FUNCTION require_terminal_delegated_turn_result() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    checked_turn uuid;
    checked_session uuid;
    spawning_request uuid;
    result_count bigint;
    correlated_result_count bigint;
    automatic_closure boolean;
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
    SELECT count(*)
      INTO result_count
      FROM session_child_result AS result
     WHERE result.spawning_tool_request_id = spawning_request;
    SELECT count(*)
      INTO correlated_result_count
      FROM session_child_result AS result
      JOIN session_delegation_event AS event
        ON event.spawning_tool_request_id = result.spawning_tool_request_id
       AND event.event_ordinal = result.event_ordinal
     WHERE result.spawning_tool_request_id = spawning_request
       AND result.outcome_kind = 'child_failed'
       AND result.content_text IS NULL
       AND event.event_kind = 'outcome_recorded'
       AND event.outcome_kind = 'child_failed'
       AND event.reason_kind = 'child_result_unavailable'
       AND event.provenance_kind = 'child_turn'
       AND event.provenance_session_id = checked_session
       AND event.provenance_turn_id = checked_turn;
    SELECT EXISTS (
        SELECT 1
          FROM automatic_reconciliation AS recovery
          JOIN turn_lifecycle AS lifecycle
            ON lifecycle.turn_id = recovery.turn_id
           AND lifecycle.session_id = recovery.session_id
         WHERE recovery.turn_id = checked_turn
           AND recovery.session_id = checked_session
           AND recovery.state_kind = 'reconciled'
           AND recovery.model_call_id = lifecycle.terminal_model_call_id
           AND lifecycle.state_kind = 'terminal'
           AND lifecycle.terminal_disposition_kind =
                'reconciliation_required'
    ) INTO automatic_closure;
    IF EXISTS (
        SELECT 1 FROM turn_lifecycle AS lifecycle
         WHERE lifecycle.turn_id = checked_turn
           AND lifecycle.session_id = checked_session
           AND lifecycle.state_kind = 'terminal'
           AND lifecycle.terminal_disposition_kind = 'reconciliation_required'
    ) AND (
        result_count <> (CASE WHEN automatic_closure THEN 1 ELSE 0 END)
        OR correlated_result_count <>
            (CASE WHEN automatic_closure THEN 1 ELSE 0 END)
    ) THEN
        RAISE EXCEPTION
            'reconciliation-required delegated child result cardinality changed'
            USING ERRCODE = '23503',
                CONSTRAINT = 'reconciling_delegated_turn_result_cardinality';
    ELSIF EXISTS (
        SELECT 1 FROM turn_lifecycle AS lifecycle
         WHERE lifecycle.turn_id = checked_turn
           AND lifecycle.session_id = checked_session
           AND lifecycle.state_kind = 'terminal'
           AND lifecycle.terminal_disposition_kind <> 'reconciliation_required'
    ) AND result_count <> 1 THEN
        RAISE EXCEPTION
            'deliverably terminal delegated child turn requires exactly one typed result'
            USING ERRCODE = '23503',
                CONSTRAINT = 'terminal_delegated_turn_result_required';
    END IF;
    RETURN NULL;
END;
$$;


--
-- Tables.
--

--
-- Name: session_delegation_parent_termination; Type: TABLE; Schema: public
--

CREATE TABLE session_delegation_parent_termination (
    spawning_tool_request_id uuid CONSTRAINT session_delegation_parent_ter_spawning_tool_request_id_not_null NOT NULL,
    root_command_id uuid NOT NULL,
    parent_session_id uuid CONSTRAINT session_delegation_parent_terminatio_parent_session_id_not_null NOT NULL,
    command_source_kind text CONSTRAINT session_delegation_parent_terminat_command_source_kind_not_null NOT NULL,
    parent_turn_id uuid,
    parent_goal_generation numeric(20,0),
    termination_kind text NOT NULL,
    source_kind text NOT NULL,
    source_spawning_tool_request_id uuid,
    CONSTRAINT session_delegation_parent_command_source_shape CHECK ((((command_source_kind = 'turn_command'::text) AND (parent_turn_id IS NOT NULL) AND (parent_goal_generation IS NULL)) OR ((command_source_kind = 'goal_command'::text) AND (parent_turn_id IS NULL) AND (parent_goal_generation IS NOT NULL)))),
    CONSTRAINT session_delegation_parent_goal_generation_positive CHECK (((parent_goal_generation IS NULL) OR ((parent_goal_generation >= (1)::numeric) AND (parent_goal_generation <= '18446744073709551615'::numeric)))),
    CONSTRAINT session_delegation_parent_termination_command_source_kind_check CHECK ((command_source_kind = ANY (ARRAY['turn_command'::text, 'goal_command'::text]))),
    CONSTRAINT session_delegation_parent_termination_source_kind_check CHECK ((source_kind = ANY (ARRAY['root'::text, 'parent_disposition'::text]))),
    CONSTRAINT session_delegation_parent_termination_source_shape CHECK ((((source_kind = 'root'::text) AND (source_spawning_tool_request_id IS NULL)) OR ((source_kind = 'parent_disposition'::text) AND (source_spawning_tool_request_id IS NOT NULL)))),
    CONSTRAINT session_delegation_parent_termination_termination_kind_check CHECK ((termination_kind = ANY (ARRAY['stopped'::text, 'cancelled'::text])))
);


--
-- Name: session_delegation_termination_cascade; Type: TABLE; Schema: public
--

CREATE TABLE session_delegation_termination_cascade (
    root_command_id uuid NOT NULL,
    root_session_id uuid NOT NULL,
    root_source_kind text CONSTRAINT session_delegation_termination_cascad_root_source_kind_not_null NOT NULL,
    root_turn_id uuid,
    root_goal_generation numeric(20,0),
    termination_kind text CONSTRAINT session_delegation_termination_cascad_termination_kind_not_null NOT NULL,
    descendant_scope text CONSTRAINT session_delegation_termination_cascad_descendant_scope_not_null NOT NULL,
    disposition_count numeric(20,0) CONSTRAINT session_delegation_termination_casca_disposition_count_not_null NOT NULL,
    CONSTRAINT session_delegation_cascade_command_source_shape CHECK ((((root_source_kind = 'turn_command'::text) AND (root_turn_id IS NOT NULL) AND (root_goal_generation IS NULL) AND (termination_kind = 'cancelled'::text)) OR ((root_source_kind = 'goal_command'::text) AND (root_turn_id IS NULL) AND (root_goal_generation IS NOT NULL) AND (termination_kind = 'stopped'::text)))),
    CONSTRAINT session_delegation_cascade_goal_generation_positive CHECK (((root_goal_generation IS NULL) OR ((root_goal_generation >= (1)::numeric) AND (root_goal_generation <= '18446744073709551615'::numeric)))),
    CONSTRAINT session_delegation_termination_cascade_descendant_scope_check CHECK ((descendant_scope = 'parent_and_descendants'::text)),
    CONSTRAINT session_delegation_termination_cascade_disposition_count_check CHECK (((disposition_count >= (0)::numeric) AND (disposition_count <= '18446744073709551615'::numeric))),
    CONSTRAINT session_delegation_termination_cascade_root_source_kind_check CHECK ((root_source_kind = ANY (ARRAY['turn_command'::text, 'goal_command'::text]))),
    CONSTRAINT session_delegation_termination_cascade_termination_kind_check CHECK ((termination_kind = ANY (ARRAY['stopped'::text, 'cancelled'::text])))
);


--
-- Name: assert_delegation_parent_termination_chain(session_delegation_parent_termination, session_delegation_termination_cascade); Type: FUNCTION; Schema: public
--

CREATE FUNCTION assert_delegation_parent_termination_chain(checked session_delegation_parent_termination, cascade session_delegation_termination_cascade) RETURNS void
    LANGUAGE plpgsql
    AS $$
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
           -- `delegation_cascade_expected_frontier` derives a nested edge's
           -- effective parent kind from the recorded prior result on its source
           -- edge, falling back to that edge's own effective kind. An
           -- already-terminal source edge therefore carries the kind that
           -- actually terminalized it, which is the earlier command's kind
           -- whenever a second descendant-scoped command of the opposite kind
           -- re-traverses the tree. Mirror that derivation exactly;
           -- `require_delegation_cascade_disposition_count` already does.
           AND checked.termination_kind = CASE source_event.outcome_kind
                WHEN 'child_stopped' THEN 'stopped'
                WHEN 'child_cancelled' THEN 'cancelled'
                WHEN 'already_terminal' THEN COALESCE(
                    (
                        SELECT CASE prior.outcome_kind
                            WHEN 'child_stopped' THEN 'stopped'
                            WHEN 'child_cancelled' THEN 'cancelled'
                            ELSE source_authority.termination_kind
                        END
                          FROM session_child_result AS prior
                         WHERE prior.spawning_tool_request_id =
                                source_event.spawning_tool_request_id
                    ),
                    source_authority.termination_kind
                )
           END
    ) THEN
        RAISE EXCEPTION 'nested delegation termination lacks its immediate parent outcome'
            USING ERRCODE = '23514',
                CONSTRAINT = 'session_delegation_parent_termination_chain';
    END IF;
END;
$$;


--
-- Name: delegation_outbox_event; Type: TABLE; Schema: public
--

CREATE TABLE delegation_outbox_event (
    event_sequence numeric(20,0) NOT NULL,
    event_kind text NOT NULL,
    storage_version smallint NOT NULL,
    session_id uuid NOT NULL,
    CONSTRAINT delegation_outbox_event_event_kind_check CHECK ((event_kind = ANY (ARRAY['delegation_update'::text, 'delegation_wake'::text]))),
    CONSTRAINT delegation_outbox_event_storage_version_check CHECK ((storage_version = 1))
);


--
-- Name: delegation_update_outbox_event; Type: TABLE; Schema: public
--

CREATE TABLE delegation_update_outbox_event (
    event_sequence numeric(20,0) NOT NULL,
    event_kind text NOT NULL,
    storage_version smallint NOT NULL,
    session_id uuid NOT NULL,
    update_kind text NOT NULL,
    spawning_tool_request_id uuid CONSTRAINT delegation_update_outbox_even_spawning_tool_request_id_not_null NOT NULL,
    child_session_id uuid,
    policy_kind text,
    on_parent_stopped text,
    on_parent_cancelled text,
    awaiting_tool_request_id uuid,
    wait_mode text,
    delegation_event_ordinal numeric(20,0),
    delegation_event_kind text,
    outcome_kind text,
    reason_kind text,
    provenance_kind text,
    provenance_session_id uuid,
    provenance_turn_id uuid,
    provenance_goal_generation numeric(20,0),
    provenance_command_id uuid,
    result_spawning_request_id uuid,
    message_id uuid,
    sender_session_id uuid,
    recipient_session_id uuid,
    message_ordinal numeric(20,0),
    content_text text,
    CONSTRAINT delegation_update_outbox_event_content_text_check CHECK (((content_text IS NULL) OR ((octet_length(content_text) >= 1) AND (octet_length(content_text) <= 1048576)))),
    CONSTRAINT delegation_update_outbox_event_event_kind_check CHECK ((event_kind = 'delegation_update'::text)),
    CONSTRAINT delegation_update_outbox_event_message_ordinal_check CHECK (((message_ordinal IS NULL) OR ((message_ordinal >= (1)::numeric) AND (message_ordinal <= '18446744073709551615'::numeric)))),
    CONSTRAINT delegation_update_outbox_event_on_parent_cancelled_check CHECK (((on_parent_cancelled IS NULL) OR (on_parent_cancelled = ANY (ARRAY['keep_running'::text, 'stop'::text, 'cancel'::text])))),
    CONSTRAINT delegation_update_outbox_event_on_parent_stopped_check CHECK (((on_parent_stopped IS NULL) OR (on_parent_stopped = ANY (ARRAY['keep_running'::text, 'stop'::text, 'cancel'::text])))),
    CONSTRAINT delegation_update_outbox_event_outcome_kind_check CHECK (((outcome_kind IS NULL) OR (outcome_kind = ANY (ARRAY['result_returned'::text, 'child_failed'::text, 'child_stopped'::text, 'child_cancelled'::text, 'continue_running'::text, 'already_terminal'::text])))),
    CONSTRAINT delegation_update_outbox_event_policy_kind_check CHECK (((policy_kind IS NULL) OR (policy_kind = ANY (ARRAY['background'::text, 'bound'::text])))),
    CONSTRAINT delegation_update_outbox_event_provenance_goal_generation_check CHECK (((provenance_goal_generation IS NULL) OR ((provenance_goal_generation >= (1)::numeric) AND (provenance_goal_generation <= '18446744073709551615'::numeric)))),
    CONSTRAINT delegation_update_outbox_event_provenance_kind_check CHECK (((provenance_kind IS NULL) OR (provenance_kind = ANY (ARRAY['child_turn'::text, 'parent_turn_command'::text, 'parent_goal_command'::text])))),
    CONSTRAINT delegation_update_outbox_event_reason_kind_check CHECK (((reason_kind IS NULL) OR (reason_kind = ANY (ARRAY['child_completed'::text, 'child_execution_failed'::text, 'child_result_unavailable'::text, 'child_cancelled'::text, 'parent_stopped_parent_and_descendants'::text, 'parent_cancelled_parent_and_descendants'::text])))),
    CONSTRAINT delegation_update_outbox_event_storage_version_check CHECK ((storage_version = 1)),
    CONSTRAINT delegation_update_outbox_event_update_kind_check CHECK ((update_kind = ANY (ARRAY['child_spawned'::text, 'child_waiting'::text, 'child_lifecycle_disposition'::text, 'child_result'::text, 'session_message'::text]))),
    CONSTRAINT delegation_update_outbox_event_wait_mode_check CHECK (((wait_mode IS NULL) OR (wait_mode = ANY (ARRAY['foreground'::text, 'background'::text])))),
    CONSTRAINT delegation_update_provenance_shape CHECK ((((provenance_kind IS NULL) AND (provenance_session_id IS NULL) AND (provenance_turn_id IS NULL) AND (provenance_goal_generation IS NULL) AND (provenance_command_id IS NULL)) OR ((provenance_kind = 'child_turn'::text) AND (provenance_session_id IS NOT NULL) AND (provenance_turn_id IS NOT NULL) AND (provenance_goal_generation IS NULL) AND (provenance_command_id IS NULL)) OR ((provenance_kind = 'parent_turn_command'::text) AND (provenance_session_id IS NOT NULL) AND (provenance_turn_id IS NOT NULL) AND (provenance_goal_generation IS NULL) AND (provenance_command_id IS NOT NULL)) OR ((provenance_kind = 'parent_goal_command'::text) AND (provenance_session_id IS NOT NULL) AND (provenance_turn_id IS NULL) AND (provenance_goal_generation IS NOT NULL) AND (provenance_command_id IS NOT NULL)))),
    CONSTRAINT delegation_update_subject_shape CHECK ((((update_kind = 'child_spawned'::text) AND (child_session_id IS NOT NULL) AND (policy_kind IS NOT NULL) AND (((policy_kind = 'background'::text) AND (on_parent_stopped IS NULL) AND (on_parent_cancelled IS NULL)) OR ((policy_kind = 'bound'::text) AND (on_parent_stopped IS NOT NULL) AND (on_parent_cancelled IS NOT NULL))) AND (awaiting_tool_request_id IS NULL) AND (wait_mode IS NULL) AND (delegation_event_ordinal IS NOT NULL) AND (delegation_event_ordinal = (1)::numeric) AND (delegation_event_kind IS NOT NULL) AND (delegation_event_kind = 'spawned'::text) AND (outcome_kind IS NULL) AND (reason_kind IS NULL) AND (provenance_kind IS NULL) AND (result_spawning_request_id IS NULL) AND (message_id IS NULL) AND (sender_session_id IS NULL) AND (recipient_session_id IS NULL) AND (message_ordinal IS NULL) AND (content_text IS NULL)) OR ((update_kind = 'child_waiting'::text) AND (child_session_id IS NOT NULL) AND (policy_kind IS NULL) AND (on_parent_stopped IS NULL) AND (on_parent_cancelled IS NULL) AND (awaiting_tool_request_id IS NOT NULL) AND (wait_mode IS NOT NULL) AND (delegation_event_ordinal IS NULL) AND (delegation_event_kind IS NULL) AND (outcome_kind IS NULL) AND (reason_kind IS NULL) AND (provenance_kind IS NULL) AND (result_spawning_request_id IS NULL) AND (message_id IS NULL) AND (sender_session_id IS NULL) AND (recipient_session_id IS NULL) AND (message_ordinal IS NULL) AND (content_text IS NULL)) OR ((update_kind = 'child_lifecycle_disposition'::text) AND (child_session_id IS NOT NULL) AND (policy_kind IS NULL) AND (on_parent_stopped IS NULL) AND (on_parent_cancelled IS NULL) AND (awaiting_tool_request_id IS NULL) AND (wait_mode IS NULL) AND (delegation_event_ordinal IS NOT NULL) AND (delegation_event_kind IS NOT NULL) AND (delegation_event_kind = 'outcome_recorded'::text) AND (outcome_kind = ANY (ARRAY['child_stopped'::text, 'child_cancelled'::text, 'already_terminal'::text, 'continue_running'::text])) AND (reason_kind = ANY (ARRAY['parent_stopped_parent_and_descendants'::text, 'parent_cancelled_parent_and_descendants'::text])) AND (provenance_kind = ANY (ARRAY['parent_turn_command'::text, 'parent_goal_command'::text])) AND (result_spawning_request_id IS NULL) AND (message_id IS NULL) AND (sender_session_id IS NULL) AND (recipient_session_id IS NULL) AND (message_ordinal IS NULL) AND (content_text IS NULL)) OR ((update_kind = 'child_result'::text) AND (child_session_id IS NOT NULL) AND (policy_kind IS NULL) AND (on_parent_stopped IS NULL) AND (on_parent_cancelled IS NULL) AND (awaiting_tool_request_id IS NULL) AND (wait_mode IS NULL) AND (delegation_event_ordinal IS NULL) AND (delegation_event_kind IS NULL) AND (outcome_kind IS NOT NULL) AND (outcome_kind = ANY (ARRAY['result_returned'::text, 'child_failed'::text, 'child_stopped'::text, 'child_cancelled'::text])) AND (reason_kind IS NOT NULL) AND (provenance_kind IS NOT NULL) AND (result_spawning_request_id IS NOT NULL) AND (result_spawning_request_id = spawning_tool_request_id) AND (message_id IS NULL) AND (sender_session_id IS NULL) AND (recipient_session_id IS NULL) AND (message_ordinal IS NULL) AND (((outcome_kind = 'result_returned'::text) AND (content_text IS NOT NULL)) OR ((outcome_kind <> 'result_returned'::text) AND (content_text IS NULL)))) OR ((update_kind = 'session_message'::text) AND (child_session_id IS NULL) AND (policy_kind IS NULL) AND (on_parent_stopped IS NULL) AND (on_parent_cancelled IS NULL) AND (awaiting_tool_request_id IS NULL) AND (wait_mode IS NULL) AND (delegation_event_ordinal IS NULL) AND (delegation_event_kind IS NULL) AND (outcome_kind IS NULL) AND (reason_kind IS NULL) AND (provenance_kind IS NULL) AND (result_spawning_request_id IS NULL) AND (message_id IS NOT NULL) AND (sender_session_id IS NOT NULL) AND (recipient_session_id IS NOT NULL) AND (sender_session_id <> recipient_session_id) AND (message_ordinal IS NOT NULL) AND (content_text IS NOT NULL))))
);


--
-- Name: delegation_wake_outbox_event; Type: TABLE; Schema: public
--

CREATE TABLE delegation_wake_outbox_event (
    event_sequence numeric(20,0) NOT NULL,
    event_kind text NOT NULL,
    storage_version smallint NOT NULL,
    session_id uuid NOT NULL,
    spawning_tool_request_id uuid NOT NULL,
    subject_kind text NOT NULL,
    result_spawning_request_id uuid,
    awaiting_tool_request_id uuid,
    message_id uuid,
    CONSTRAINT delegation_wake_outbox_event_event_kind_check CHECK ((event_kind = 'delegation_wake'::text)),
    CONSTRAINT delegation_wake_outbox_event_storage_version_check CHECK ((storage_version = 1)),
    CONSTRAINT delegation_wake_outbox_event_subject_kind_check CHECK ((subject_kind = ANY (ARRAY['result'::text, 'message'::text]))),
    CONSTRAINT delegation_wake_subject_shape CHECK ((((subject_kind = 'result'::text) AND (result_spawning_request_id IS NOT NULL) AND (result_spawning_request_id = spawning_tool_request_id) AND (message_id IS NULL)) OR ((subject_kind = 'message'::text) AND (result_spawning_request_id IS NULL) AND (awaiting_tool_request_id IS NULL) AND (message_id IS NOT NULL))))
);


--
-- Name: session_child_result; Type: TABLE; Schema: public
--

CREATE TABLE session_child_result (
    spawning_tool_request_id uuid NOT NULL,
    event_ordinal numeric(20,0) NOT NULL,
    event_kind text NOT NULL,
    outcome_kind text NOT NULL,
    content_text text,
    CONSTRAINT session_child_result_content_text_check CHECK (((content_text IS NULL) OR ((octet_length(content_text) >= 1) AND (octet_length(content_text) <= 1048576)))),
    CONSTRAINT session_child_result_event_kind_check CHECK ((event_kind = 'outcome_recorded'::text)),
    CONSTRAINT session_child_result_outcome_kind_check CHECK ((outcome_kind = ANY (ARRAY['result_returned'::text, 'child_failed'::text, 'child_stopped'::text, 'child_cancelled'::text]))),
    CONSTRAINT session_child_result_shape CHECK ((((outcome_kind = 'result_returned'::text) AND (content_text IS NOT NULL)) OR ((outcome_kind <> 'result_returned'::text) AND (content_text IS NULL))))
);


--
-- Name: session_child_result_delivery; Type: TABLE; Schema: public
--

CREATE TABLE session_child_result_delivery (
    awaiting_tool_request_id uuid NOT NULL,
    spawning_tool_request_id uuid NOT NULL,
    parent_session_id uuid NOT NULL,
    delivery_sequence numeric(20,0),
    delivery_kind text,
    CONSTRAINT session_child_result_delivery_delivery_kind_check CHECK ((delivery_kind = 'background_result'::text)),
    CONSTRAINT session_child_result_delivery_sequence_shape CHECK ((((delivery_sequence IS NULL) AND (delivery_kind IS NULL)) OR ((delivery_sequence IS NOT NULL) AND (delivery_kind IS NOT NULL) AND (delivery_kind = 'background_result'::text))))
);


--
-- Name: session_delegation; Type: TABLE; Schema: public
--

CREATE TABLE session_delegation (
    spawning_tool_request_id uuid NOT NULL,
    parent_session_id uuid NOT NULL,
    parent_turn_id uuid NOT NULL,
    child_session_id uuid NOT NULL,
    policy_kind text NOT NULL,
    on_parent_stopped text,
    on_parent_cancelled text,
    CONSTRAINT session_delegation_distinct_sessions CHECK ((parent_session_id <> child_session_id)),
    CONSTRAINT session_delegation_on_parent_cancelled_check CHECK (((on_parent_cancelled IS NULL) OR (on_parent_cancelled = ANY (ARRAY['keep_running'::text, 'stop'::text, 'cancel'::text])))),
    CONSTRAINT session_delegation_on_parent_stopped_check CHECK (((on_parent_stopped IS NULL) OR (on_parent_stopped = ANY (ARRAY['keep_running'::text, 'stop'::text, 'cancel'::text])))),
    CONSTRAINT session_delegation_policy_kind_check CHECK ((policy_kind = ANY (ARRAY['background'::text, 'bound'::text]))),
    CONSTRAINT session_delegation_policy_shape CHECK ((((policy_kind = 'background'::text) AND (on_parent_stopped IS NULL) AND (on_parent_cancelled IS NULL)) OR ((policy_kind = 'bound'::text) AND (on_parent_stopped IS NOT NULL) AND (on_parent_cancelled IS NOT NULL))))
);


--
-- Name: session_delegation_event; Type: TABLE; Schema: public
--

CREATE TABLE session_delegation_event (
    spawning_tool_request_id uuid NOT NULL,
    event_ordinal numeric(20,0) NOT NULL,
    event_kind text NOT NULL,
    outcome_kind text,
    reason_kind text,
    provenance_kind text NOT NULL,
    provenance_session_id uuid NOT NULL,
    provenance_turn_id uuid,
    provenance_goal_generation numeric(20,0),
    provenance_tool_request_id uuid,
    provenance_command_id uuid,
    CONSTRAINT session_delegation_event_event_kind_check CHECK ((event_kind = ANY (ARRAY['spawned'::text, 'message_delivered'::text, 'outcome_recorded'::text]))),
    CONSTRAINT session_delegation_event_event_ordinal_check CHECK (((event_ordinal >= (1)::numeric) AND (event_ordinal <= '18446744073709551615'::numeric))),
    CONSTRAINT session_delegation_event_goal_generation_positive CHECK (((provenance_goal_generation IS NULL) OR ((provenance_goal_generation >= (1)::numeric) AND (provenance_goal_generation <= '18446744073709551615'::numeric)))),
    CONSTRAINT session_delegation_event_outcome_kind_check CHECK (((outcome_kind IS NULL) OR (outcome_kind = ANY (ARRAY['result_returned'::text, 'child_failed'::text, 'child_stopped'::text, 'child_cancelled'::text, 'continue_running'::text, 'already_terminal'::text])))),
    CONSTRAINT session_delegation_event_provenance_kind_check CHECK ((provenance_kind = ANY (ARRAY['tool_request'::text, 'child_turn'::text, 'parent_turn_command'::text, 'parent_goal_command'::text]))),
    CONSTRAINT session_delegation_event_provenance_shape CHECK ((((provenance_kind = 'tool_request'::text) AND (provenance_turn_id IS NOT NULL) AND (provenance_goal_generation IS NULL) AND (provenance_tool_request_id IS NOT NULL) AND (provenance_command_id IS NULL)) OR ((provenance_kind = 'child_turn'::text) AND (provenance_turn_id IS NOT NULL) AND (provenance_goal_generation IS NULL) AND (provenance_tool_request_id IS NULL) AND (provenance_command_id IS NULL)) OR ((provenance_kind = 'parent_turn_command'::text) AND (provenance_turn_id IS NOT NULL) AND (provenance_goal_generation IS NULL) AND (provenance_tool_request_id IS NULL) AND (provenance_command_id IS NOT NULL)) OR ((provenance_kind = 'parent_goal_command'::text) AND (provenance_turn_id IS NULL) AND (provenance_goal_generation IS NOT NULL) AND (provenance_tool_request_id IS NULL) AND (provenance_command_id IS NOT NULL)))),
    CONSTRAINT session_delegation_event_reason_kind_check CHECK (((reason_kind IS NULL) OR (reason_kind = ANY (ARRAY['child_completed'::text, 'child_execution_failed'::text, 'child_result_unavailable'::text, 'child_cancelled'::text, 'parent_stopped_parent_and_descendants'::text, 'parent_cancelled_parent_and_descendants'::text])))),
    CONSTRAINT session_delegation_event_shape CHECK ((((event_kind = ANY (ARRAY['spawned'::text, 'message_delivered'::text])) AND (outcome_kind IS NULL) AND (reason_kind IS NULL)) OR ((event_kind = 'outcome_recorded'::text) AND (outcome_kind IS NOT NULL) AND (reason_kind IS NOT NULL)))),
    CONSTRAINT session_delegation_spawn_ordinal CHECK (((event_kind = 'spawned'::text) = (event_ordinal = (1)::numeric))),
    CONSTRAINT session_delegation_spawn_provenance CHECK (((event_kind <> 'spawned'::text) OR ((provenance_kind = 'tool_request'::text) AND (provenance_tool_request_id = spawning_tool_request_id))))
);


--
-- Name: session_delegation_initial_task; Type: TABLE; Schema: public
--

CREATE TABLE session_delegation_initial_task (
    spawning_tool_request_id uuid CONSTRAINT session_delegation_initial_ta_spawning_tool_request_id_not_null NOT NULL,
    child_session_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    semantic_entry_id uuid NOT NULL,
    admission_position numeric(20,0) NOT NULL,
    defaults_version numeric(20,0) NOT NULL,
    requested_model_kind text NOT NULL,
    requested_direct_model_selection_id uuid,
    requested_model_alias_id uuid,
    frozen_model_kind text NOT NULL,
    frozen_direct_model_selection_id uuid,
    frozen_model_alias_id uuid,
    frozen_alias_selected_direct_id uuid,
    task_content text NOT NULL,
    CONSTRAINT session_delegation_initial_task_admission_position_check CHECK ((admission_position = (1)::numeric)),
    CONSTRAINT session_delegation_initial_task_defaults_version_check CHECK ((defaults_version = (1)::numeric)),
    CONSTRAINT session_delegation_initial_task_frozen_model_shape CHECK ((((frozen_model_kind = 'direct'::text) AND (frozen_direct_model_selection_id IS NOT NULL) AND (frozen_model_alias_id IS NULL) AND (frozen_alias_selected_direct_id IS NULL)) OR ((frozen_model_kind = 'frozen_alias'::text) AND (frozen_direct_model_selection_id IS NULL) AND (frozen_model_alias_id IS NOT NULL) AND (frozen_alias_selected_direct_id IS NOT NULL)))),
    CONSTRAINT session_delegation_initial_task_requested_model_shape CHECK ((((requested_model_kind = 'direct'::text) AND (requested_direct_model_selection_id IS NOT NULL) AND (requested_model_alias_id IS NULL)) OR ((requested_model_kind = 'alias'::text) AND (requested_direct_model_selection_id IS NULL) AND (requested_model_alias_id IS NOT NULL)))),
    CONSTRAINT session_delegation_initial_task_task_content_check CHECK (((task_content <> ''::text) AND (octet_length(task_content) <= 1048576)))
);


--
-- Name: session_delegation_logical_terminal; Type: TABLE; Schema: public
--

CREATE TABLE session_delegation_logical_terminal (
    spawning_tool_request_id uuid CONSTRAINT session_delegation_logical_te_spawning_tool_request_id_not_null NOT NULL,
    child_session_id uuid NOT NULL,
    child_turn_id uuid NOT NULL,
    root_command_id uuid NOT NULL,
    terminal_frontier_id uuid CONSTRAINT session_delegation_logical_termin_terminal_frontier_id_not_null NOT NULL,
    disposition_kind text NOT NULL,
    CONSTRAINT session_delegation_logical_terminal_disposition_kind_check CHECK ((disposition_kind = ANY (ARRAY['stopped'::text, 'cancelled'::text])))
);


--
-- Name: session_delegation_message_rejection; Type: TABLE; Schema: public
--

CREATE TABLE session_delegation_message_rejection (
    tool_request_id uuid NOT NULL,
    message_id uuid NOT NULL,
    rejection_kind text NOT NULL,
    spawning_tool_request_id uuid,
    transition_failure_kind text,
    CONSTRAINT session_delegation_message_reject_transition_failure_kind_check CHECK (((transition_failure_kind IS NULL) OR (transition_failure_kind = ANY (ARRAY['same_session'::text, 'already_terminal'::text, 'missing_spawn_event'::text, 'invalid_provenance'::text, 'descendants_not_selected'::text, 'duplicate_message_identity'::text, 'conflicting_message_replay'::text, 'duplicate_outcome_authority'::text, 'outcome_reason_mismatch'::text, 'event_ordinal_exhausted'::text])))),
    CONSTRAINT session_delegation_message_rejection_rejection_kind_check CHECK ((rejection_kind = ANY (ARRAY['relationship_not_found'::text, 'message_identity_collision'::text, 'delivery_sequence_exhausted'::text, 'transition'::text]))),
    CONSTRAINT session_delegation_message_rejection_shape CHECK ((((rejection_kind = 'transition'::text) AND (spawning_tool_request_id IS NOT NULL) AND (transition_failure_kind IS NOT NULL)) OR ((rejection_kind <> 'transition'::text) AND (spawning_tool_request_id IS NULL) AND (transition_failure_kind IS NULL))))
);


--
-- Name: session_delegation_wait; Type: TABLE; Schema: public
--

CREATE TABLE session_delegation_wait (
    awaiting_tool_request_id uuid NOT NULL,
    spawning_tool_request_id uuid NOT NULL,
    parent_session_id uuid NOT NULL,
    parent_turn_id uuid NOT NULL,
    child_session_id uuid NOT NULL,
    wait_mode text NOT NULL,
    CONSTRAINT session_delegation_wait_check CHECK ((awaiting_tool_request_id <> spawning_tool_request_id)),
    CONSTRAINT session_delegation_wait_wait_mode_check CHECK ((wait_mode = ANY (ARRAY['foreground'::text, 'background'::text])))
);


--
-- Name: session_delegation_wait_rejection; Type: TABLE; Schema: public
--

CREATE TABLE session_delegation_wait_rejection (
    tool_request_id uuid NOT NULL,
    rejection_kind text NOT NULL,
    spawning_tool_request_id uuid,
    transition_failure_kind text,
    CONSTRAINT session_delegation_wait_rejection_rejection_kind_check CHECK ((rejection_kind = ANY (ARRAY['relationship_not_found'::text, 'delivery_sequence_exhausted'::text, 'transition'::text]))),
    CONSTRAINT session_delegation_wait_rejection_shape CHECK ((((rejection_kind = 'transition'::text) AND (spawning_tool_request_id IS NOT NULL) AND (transition_failure_kind IS NOT NULL)) OR ((rejection_kind <> 'transition'::text) AND (spawning_tool_request_id IS NULL) AND (transition_failure_kind IS NULL)))),
    CONSTRAINT session_delegation_wait_rejection_transition_failure_kind_check CHECK (((transition_failure_kind IS NULL) OR (transition_failure_kind = ANY (ARRAY['same_session'::text, 'already_terminal'::text, 'missing_spawn_event'::text, 'invalid_provenance'::text, 'descendants_not_selected'::text, 'duplicate_message_identity'::text, 'conflicting_message_replay'::text, 'duplicate_outcome_authority'::text, 'outcome_reason_mismatch'::text, 'event_ordinal_exhausted'::text]))))
);


--
-- Name: session_delegation_wake_turn_origin; Type: TABLE; Schema: public
--

CREATE TABLE session_delegation_wake_turn_origin (
    turn_id uuid NOT NULL,
    recipient_session_id uuid CONSTRAINT session_delegation_wake_turn_orig_recipient_session_id_not_null NOT NULL,
    admission_position numeric(20,0) NOT NULL,
    first_delivery_sequence numeric(20,0) CONSTRAINT session_delegation_wake_turn_o_first_delivery_sequence_not_null NOT NULL,
    through_delivery_sequence numeric(20,0) CONSTRAINT session_delegation_wake_turn_through_delivery_sequence_not_null NOT NULL,
    defaults_version numeric(20,0) NOT NULL,
    requested_model_kind text CONSTRAINT session_delegation_wake_turn_orig_requested_model_kind_not_null NOT NULL,
    requested_direct_model_selection_id uuid,
    requested_model_alias_id uuid,
    frozen_model_kind text NOT NULL,
    frozen_direct_model_selection_id uuid,
    frozen_model_alias_id uuid,
    frozen_alias_selected_direct_id uuid,
    CONSTRAINT session_delegation_wake_delivery_range CHECK ((first_delivery_sequence <= through_delivery_sequence)),
    CONSTRAINT session_delegation_wake_frozen_model_shape CHECK ((((frozen_model_kind = 'direct'::text) AND (frozen_direct_model_selection_id IS NOT NULL) AND (frozen_model_alias_id IS NULL) AND (frozen_alias_selected_direct_id IS NULL)) OR ((frozen_model_kind = 'frozen_alias'::text) AND (frozen_direct_model_selection_id IS NULL) AND (frozen_model_alias_id IS NOT NULL) AND (frozen_alias_selected_direct_id IS NOT NULL)))),
    CONSTRAINT session_delegation_wake_requested_model_shape CHECK ((((requested_model_kind = 'direct'::text) AND (requested_direct_model_selection_id IS NOT NULL) AND (requested_model_alias_id IS NULL)) OR ((requested_model_kind = 'alias'::text) AND (requested_direct_model_selection_id IS NULL) AND (requested_model_alias_id IS NOT NULL)))),
    CONSTRAINT session_delegation_wake_turn_or_through_delivery_sequence_check CHECK (((through_delivery_sequence >= (1)::numeric) AND (through_delivery_sequence <= '18446744073709551615'::numeric))),
    CONSTRAINT session_delegation_wake_turn_orig_first_delivery_sequence_check CHECK (((first_delivery_sequence >= (1)::numeric) AND (first_delivery_sequence <= '18446744073709551615'::numeric))),
    CONSTRAINT session_delegation_wake_turn_origin_admission_position_check CHECK (((admission_position >= (1)::numeric) AND (admission_position <= '18446744073709551615'::numeric))),
    CONSTRAINT session_delegation_wake_turn_origin_defaults_version_check CHECK (((defaults_version >= (1)::numeric) AND (defaults_version <= '18446744073709551615'::numeric)))
);


--
-- Name: session_message; Type: TABLE; Schema: public
--

CREATE TABLE session_message (
    message_id uuid NOT NULL,
    spawning_tool_request_id uuid NOT NULL,
    event_ordinal numeric(20,0) NOT NULL,
    event_kind text NOT NULL,
    direction text NOT NULL,
    content_text text NOT NULL,
    CONSTRAINT session_message_content_text_check CHECK (((octet_length(content_text) >= 1) AND (octet_length(content_text) <= 1048576))),
    CONSTRAINT session_message_direction_check CHECK ((direction = ANY (ARRAY['parent_to_child'::text, 'child_to_parent'::text]))),
    CONSTRAINT session_message_event_kind_check CHECK ((event_kind = 'message_delivered'::text))
);


--
-- Name: session_message_delivery; Type: TABLE; Schema: public
--

CREATE TABLE session_message_delivery (
    message_id uuid NOT NULL,
    spawning_tool_request_id uuid NOT NULL,
    recipient_session_id uuid NOT NULL,
    delivery_sequence numeric(20,0) NOT NULL,
    delivery_kind text NOT NULL,
    CONSTRAINT session_message_delivery_delivery_kind_check CHECK ((delivery_kind = 'message'::text))
);


--
-- Name: session_pending_delivery; Type: TABLE; Schema: public
--

CREATE TABLE session_pending_delivery (
    recipient_session_id uuid NOT NULL,
    delivery_sequence numeric(20,0) NOT NULL,
    delivery_kind text NOT NULL,
    CONSTRAINT session_pending_delivery_delivery_kind_check CHECK ((delivery_kind = ANY (ARRAY['message'::text, 'background_result'::text]))),
    CONSTRAINT session_pending_delivery_sequence_positive CHECK (((delivery_sequence >= (1)::numeric) AND (delivery_sequence <= '18446744073709551615'::numeric)))
);


--
-- Constraints.
--

--
-- Name: delegation_outbox_event delegation_outbox_event_event_sequence_event_kind_storage_v_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY delegation_outbox_event
    ADD CONSTRAINT delegation_outbox_event_event_sequence_event_kind_storage_v_key UNIQUE (event_sequence, event_kind, storage_version, session_id);


--
-- Name: delegation_outbox_event delegation_outbox_event_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY delegation_outbox_event
    ADD CONSTRAINT delegation_outbox_event_pkey PRIMARY KEY (event_sequence);


--
-- Name: delegation_update_outbox_event delegation_update_outbox_event_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY delegation_update_outbox_event
    ADD CONSTRAINT delegation_update_outbox_event_pkey PRIMARY KEY (event_sequence);


--
-- Name: delegation_wake_outbox_event delegation_wake_outbox_event_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY delegation_wake_outbox_event
    ADD CONSTRAINT delegation_wake_outbox_event_pkey PRIMARY KEY (event_sequence);


--
-- Name: session_child_result_delivery session_child_result_delivery_awaiting_tool_request_id_spaw_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_child_result_delivery
    ADD CONSTRAINT session_child_result_delivery_awaiting_tool_request_id_spaw_key UNIQUE (awaiting_tool_request_id, spawning_tool_request_id, parent_session_id);


--
-- Name: session_child_result_delivery session_child_result_delivery_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_child_result_delivery
    ADD CONSTRAINT session_child_result_delivery_pkey PRIMARY KEY (awaiting_tool_request_id);


--
-- Name: session_child_result session_child_result_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_child_result
    ADD CONSTRAINT session_child_result_pkey PRIMARY KEY (spawning_tool_request_id);


--
-- Name: session_delegation session_delegation_child_session_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation
    ADD CONSTRAINT session_delegation_child_session_id_key UNIQUE (child_session_id);


--
-- Name: session_delegation_event session_delegation_event_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation_event
    ADD CONSTRAINT session_delegation_event_pkey PRIMARY KEY (spawning_tool_request_id, event_ordinal);


--
-- Name: session_delegation_event session_delegation_event_spawning_tool_request_id_event_or_key1; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation_event
    ADD CONSTRAINT session_delegation_event_spawning_tool_request_id_event_or_key1 UNIQUE (spawning_tool_request_id, event_ordinal, event_kind, outcome_kind);


--
-- Name: session_delegation_event session_delegation_event_spawning_tool_request_id_event_ord_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation_event
    ADD CONSTRAINT session_delegation_event_spawning_tool_request_id_event_ord_key UNIQUE (spawning_tool_request_id, event_ordinal, event_kind);


--
-- Name: session_delegation_initial_task session_delegation_initial_ta_spawning_tool_request_id_chi_key1; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation_initial_task
    ADD CONSTRAINT session_delegation_initial_ta_spawning_tool_request_id_chi_key1 UNIQUE (spawning_tool_request_id, child_session_id, semantic_entry_id);


--
-- Name: session_delegation_initial_task session_delegation_initial_ta_spawning_tool_request_id_chil_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation_initial_task
    ADD CONSTRAINT session_delegation_initial_ta_spawning_tool_request_id_chil_key UNIQUE (spawning_tool_request_id, child_session_id, turn_id);


--
-- Name: session_delegation_initial_task session_delegation_initial_ta_turn_id_child_session_id_admi_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation_initial_task
    ADD CONSTRAINT session_delegation_initial_ta_turn_id_child_session_id_admi_key UNIQUE (turn_id, child_session_id, admission_position);


--
-- Name: session_delegation_initial_task session_delegation_initial_task_child_session_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation_initial_task
    ADD CONSTRAINT session_delegation_initial_task_child_session_id_key UNIQUE (child_session_id);


--
-- Name: session_delegation_initial_task session_delegation_initial_task_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation_initial_task
    ADD CONSTRAINT session_delegation_initial_task_pkey PRIMARY KEY (spawning_tool_request_id);


--
-- Name: session_delegation_initial_task session_delegation_initial_task_semantic_entry_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation_initial_task
    ADD CONSTRAINT session_delegation_initial_task_semantic_entry_id_key UNIQUE (semantic_entry_id);


--
-- Name: session_delegation_initial_task session_delegation_initial_task_turn_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation_initial_task
    ADD CONSTRAINT session_delegation_initial_task_turn_id_key UNIQUE (turn_id);


--
-- Name: session_delegation_logical_terminal session_delegation_logical_te_spawning_tool_request_id_root_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation_logical_terminal
    ADD CONSTRAINT session_delegation_logical_te_spawning_tool_request_id_root_key UNIQUE (spawning_tool_request_id, root_command_id);


--
-- Name: session_delegation_logical_terminal session_delegation_logical_terminal_child_session_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation_logical_terminal
    ADD CONSTRAINT session_delegation_logical_terminal_child_session_id_key UNIQUE (child_session_id);


--
-- Name: session_delegation_logical_terminal session_delegation_logical_terminal_child_turn_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation_logical_terminal
    ADD CONSTRAINT session_delegation_logical_terminal_child_turn_id_key UNIQUE (child_turn_id);


--
-- Name: session_delegation_logical_terminal session_delegation_logical_terminal_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation_logical_terminal
    ADD CONSTRAINT session_delegation_logical_terminal_pkey PRIMARY KEY (spawning_tool_request_id);


--
-- Name: session_delegation_logical_terminal session_delegation_logical_terminal_terminal_frontier_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation_logical_terminal
    ADD CONSTRAINT session_delegation_logical_terminal_terminal_frontier_id_key UNIQUE (terminal_frontier_id);


--
-- Name: session_delegation_message_rejection session_delegation_message_rejection_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation_message_rejection
    ADD CONSTRAINT session_delegation_message_rejection_pkey PRIMARY KEY (tool_request_id);


--
-- Name: session_delegation_parent_termination session_delegation_parent_termination_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation_parent_termination
    ADD CONSTRAINT session_delegation_parent_termination_pkey PRIMARY KEY (spawning_tool_request_id, root_command_id);


--
-- Name: session_delegation session_delegation_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation
    ADD CONSTRAINT session_delegation_pkey PRIMARY KEY (spawning_tool_request_id);


--
-- Name: session_delegation session_delegation_spawning_tool_request_id_child_session_i_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation
    ADD CONSTRAINT session_delegation_spawning_tool_request_id_child_session_i_key UNIQUE (spawning_tool_request_id, child_session_id);


--
-- Name: session_delegation session_delegation_spawning_tool_request_id_parent_session__key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation
    ADD CONSTRAINT session_delegation_spawning_tool_request_id_parent_session__key UNIQUE (spawning_tool_request_id, parent_session_id);


--
-- Name: session_delegation session_delegation_spawning_tool_request_id_parent_session_key1; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation
    ADD CONSTRAINT session_delegation_spawning_tool_request_id_parent_session_key1 UNIQUE (spawning_tool_request_id, parent_session_id, child_session_id);


--
-- Name: session_delegation session_delegation_spawning_tool_request_id_parent_turn_id__key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation
    ADD CONSTRAINT session_delegation_spawning_tool_request_id_parent_turn_id__key UNIQUE (spawning_tool_request_id, parent_turn_id, parent_session_id);


--
-- Name: session_delegation_termination_cascade session_delegation_termination_cascade_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation_termination_cascade
    ADD CONSTRAINT session_delegation_termination_cascade_pkey PRIMARY KEY (root_command_id);


--
-- Name: session_delegation_wait session_delegation_wait_attempt_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation_wait
    ADD CONSTRAINT session_delegation_wait_attempt_key UNIQUE (awaiting_tool_request_id, spawning_tool_request_id, child_session_id);


--
-- Name: session_delegation_wait session_delegation_wait_awaiting_tool_request_id_spawning_t_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation_wait
    ADD CONSTRAINT session_delegation_wait_awaiting_tool_request_id_spawning_t_key UNIQUE (awaiting_tool_request_id, spawning_tool_request_id);


--
-- Name: session_delegation_wait session_delegation_wait_parent_turn_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation_wait
    ADD CONSTRAINT session_delegation_wait_parent_turn_key UNIQUE (awaiting_tool_request_id, parent_turn_id, parent_session_id);


--
-- Name: session_delegation_wait session_delegation_wait_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation_wait
    ADD CONSTRAINT session_delegation_wait_pkey PRIMARY KEY (awaiting_tool_request_id);


--
-- Name: session_delegation_wait_rejection session_delegation_wait_rejection_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation_wait_rejection
    ADD CONSTRAINT session_delegation_wait_rejection_pkey PRIMARY KEY (tool_request_id);


--
-- Name: session_delegation_wait session_delegation_wait_result_delivery_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation_wait
    ADD CONSTRAINT session_delegation_wait_result_delivery_key UNIQUE (awaiting_tool_request_id, spawning_tool_request_id, parent_session_id);


--
-- Name: session_delegation_wake_turn_origin session_delegation_wake_turn__recipient_session_id_through__key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation_wake_turn_origin
    ADD CONSTRAINT session_delegation_wake_turn__recipient_session_id_through__key UNIQUE (recipient_session_id, through_delivery_sequence);


--
-- Name: session_delegation_wake_turn_origin session_delegation_wake_turn__turn_id_recipient_session_id__key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation_wake_turn_origin
    ADD CONSTRAINT session_delegation_wake_turn__turn_id_recipient_session_id__key UNIQUE (turn_id, recipient_session_id, admission_position);


--
-- Name: session_delegation_wake_turn_origin session_delegation_wake_turn_origin_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation_wake_turn_origin
    ADD CONSTRAINT session_delegation_wake_turn_origin_pkey PRIMARY KEY (turn_id);


--
-- Name: session_message_delivery session_message_delivery_message_id_recipient_session_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_message_delivery
    ADD CONSTRAINT session_message_delivery_message_id_recipient_session_id_key UNIQUE (message_id, recipient_session_id);


--
-- Name: session_message_delivery session_message_delivery_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_message_delivery
    ADD CONSTRAINT session_message_delivery_pkey PRIMARY KEY (message_id);


--
-- Name: session_message session_message_message_id_spawning_tool_request_id_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_message
    ADD CONSTRAINT session_message_message_id_spawning_tool_request_id_key UNIQUE (message_id, spawning_tool_request_id);


--
-- Name: session_message session_message_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_message
    ADD CONSTRAINT session_message_pkey PRIMARY KEY (message_id);


--
-- Name: session_message session_message_spawning_tool_request_id_event_ordinal_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_message
    ADD CONSTRAINT session_message_spawning_tool_request_id_event_ordinal_key UNIQUE (spawning_tool_request_id, event_ordinal);


--
-- Name: session_pending_delivery session_pending_delivery_pkey; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_pending_delivery
    ADD CONSTRAINT session_pending_delivery_pkey PRIMARY KEY (recipient_session_id, delivery_sequence);


--
-- Name: session_pending_delivery session_pending_delivery_recipient_session_id_delivery_sequ_key; Type: CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_pending_delivery
    ADD CONSTRAINT session_pending_delivery_recipient_session_id_delivery_sequ_key UNIQUE (recipient_session_id, delivery_sequence, delivery_kind);


--
-- Indexes.
--

--
-- Name: delegation_child_result_update_once; Type: INDEX; Schema: public
--

CREATE UNIQUE INDEX delegation_child_result_update_once ON delegation_update_outbox_event USING btree (result_spawning_request_id) WHERE (update_kind = 'child_result'::text);


--
-- Name: delegation_child_spawned_update_once; Type: INDEX; Schema: public
--

CREATE UNIQUE INDEX delegation_child_spawned_update_once ON delegation_update_outbox_event USING btree (spawning_tool_request_id) WHERE (update_kind = 'child_spawned'::text);


--
-- Name: delegation_child_waiting_update_once; Type: INDEX; Schema: public
--

CREATE UNIQUE INDEX delegation_child_waiting_update_once ON delegation_update_outbox_event USING btree (awaiting_tool_request_id) WHERE (update_kind = 'child_waiting'::text);


--
-- Name: delegation_initial_result_wake_once; Type: INDEX; Schema: public
--

CREATE UNIQUE INDEX delegation_initial_result_wake_once ON delegation_wake_outbox_event USING btree (result_spawning_request_id) WHERE ((subject_kind = 'result'::text) AND (awaiting_tool_request_id IS NULL));


--
-- Name: delegation_late_wait_wake_once; Type: INDEX; Schema: public
--

CREATE UNIQUE INDEX delegation_late_wait_wake_once ON delegation_wake_outbox_event USING btree (awaiting_tool_request_id) WHERE ((subject_kind = 'result'::text) AND (awaiting_tool_request_id IS NOT NULL));


--
-- Name: delegation_lifecycle_update_once; Type: INDEX; Schema: public
--

CREATE UNIQUE INDEX delegation_lifecycle_update_once ON delegation_update_outbox_event USING btree (spawning_tool_request_id, delegation_event_ordinal, session_id) WHERE (update_kind = 'child_lifecycle_disposition'::text);


--
-- Name: delegation_message_wake_once; Type: INDEX; Schema: public
--

CREATE UNIQUE INDEX delegation_message_wake_once ON delegation_wake_outbox_event USING btree (message_id) WHERE (subject_kind = 'message'::text);


--
-- Name: delegation_outbox_event_by_session_sequence; Type: INDEX; Schema: public
--

CREATE INDEX delegation_outbox_event_by_session_sequence ON delegation_outbox_event USING btree (session_id, event_sequence);


--
-- Name: delegation_session_message_update_once; Type: INDEX; Schema: public
--

CREATE UNIQUE INDEX delegation_session_message_update_once ON delegation_update_outbox_event USING btree (message_id) WHERE (update_kind = 'session_message'::text);


--
-- Name: session_delegation_by_parent; Type: INDEX; Schema: public
--

CREATE INDEX session_delegation_by_parent ON session_delegation USING btree (parent_session_id, spawning_tool_request_id);


--
-- Name: session_delegation_child_outcome_authority_once; Type: INDEX; Schema: public
--

CREATE UNIQUE INDEX session_delegation_child_outcome_authority_once ON session_delegation_event USING btree (spawning_tool_request_id, provenance_session_id, provenance_turn_id) WHERE ((event_kind = 'outcome_recorded'::text) AND (provenance_kind = 'child_turn'::text));


--
-- Name: session_delegation_message_request_once; Type: INDEX; Schema: public
--

CREATE UNIQUE INDEX session_delegation_message_request_once ON session_delegation_event USING btree (provenance_tool_request_id) WHERE (event_kind = 'message_delivered'::text);


--
-- Name: session_delegation_parent_outcome_authority_once; Type: INDEX; Schema: public
--

CREATE UNIQUE INDEX session_delegation_parent_outcome_authority_once ON session_delegation_event USING btree (spawning_tool_request_id, provenance_command_id) WHERE ((event_kind = 'outcome_recorded'::text) AND (provenance_kind = ANY (ARRAY['parent_turn_command'::text, 'parent_goal_command'::text])));


--
-- Name: session_delegation_spawn_once; Type: INDEX; Schema: public
--

CREATE UNIQUE INDEX session_delegation_spawn_once ON session_delegation_event USING btree (spawning_tool_request_id) WHERE (event_kind = 'spawned'::text);


--
-- Name: session_delegation_termination_by_root; Type: INDEX; Schema: public
--

CREATE INDEX session_delegation_termination_by_root ON session_delegation_parent_termination USING btree (root_command_id, spawning_tool_request_id);


--
-- Name: session_delegation_wait_by_relation; Type: INDEX; Schema: public
--

CREATE INDEX session_delegation_wait_by_relation ON session_delegation_wait USING btree (spawning_tool_request_id, awaiting_tool_request_id);


--
-- Triggers.
--

--
-- Name: submit_input_command applied_turn_command_requires_delegation_cascade; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER applied_turn_command_requires_delegation_cascade AFTER INSERT OR UPDATE ON submit_input_command DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_applied_turn_command_delegation_cascade();


--
-- Name: session_child_result_delivery child_result_delivery_zz_closes_waits; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER child_result_delivery_zz_closes_waits AFTER INSERT OR DELETE OR UPDATE ON session_child_result_delivery DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_session_child_result_wait_deliveries();


--
-- Name: session_model_credential_record delegated_session_credential_purpose; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER delegated_session_credential_purpose AFTER INSERT ON session_model_credential_record DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_delegated_session_credential_purpose();


--
-- Name: session_delegation_termination_cascade delegation_cascade_requires_disposition_count; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER delegation_cascade_requires_disposition_count AFTER INSERT OR UPDATE ON session_delegation_termination_cascade DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_delegation_cascade_disposition_count();


--
-- Name: session_delegation_parent_termination delegation_disposition_requires_cascade_count; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER delegation_disposition_requires_cascade_count AFTER INSERT OR DELETE OR UPDATE ON session_delegation_parent_termination DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_delegation_cascade_disposition_count();


--
-- Name: session_delegation_initial_task delegation_initial_task_zz_requires_terminal_result; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER delegation_initial_task_zz_requires_terminal_result AFTER INSERT OR DELETE OR UPDATE ON session_delegation_initial_task DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_terminal_delegated_turn_result();


--
-- Name: delegation_outbox_event delegation_outbox_event_allocates_sequence; Type: TRIGGER; Schema: public
--

CREATE TRIGGER delegation_outbox_event_allocates_sequence BEFORE INSERT ON delegation_outbox_event FOR EACH ROW EXECUTE FUNCTION allocate_outbox_event_sequence();


--
-- Name: delegation_outbox_event delegation_outbox_event_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER delegation_outbox_event_cannot_be_truncated BEFORE TRUNCATE ON delegation_outbox_event FOR EACH STATEMENT EXECUTE FUNCTION reject_outbox_table_truncate();


--
-- Name: delegation_outbox_event delegation_outbox_event_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER delegation_outbox_event_is_append_only BEFORE DELETE OR UPDATE ON delegation_outbox_event FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: delegation_outbox_event delegation_outbox_event_requires_typed_record; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER delegation_outbox_event_requires_typed_record AFTER INSERT ON delegation_outbox_event DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_delegation_outbox_event_typed_record();


--
-- Name: delegation_outbox_event delegation_outbox_event_updates_timeline_fact; Type: TRIGGER; Schema: public
--

CREATE TRIGGER delegation_outbox_event_updates_timeline_fact AFTER INSERT ON delegation_outbox_event FOR EACH ROW EXECUTE FUNCTION append_session_timeline_event_fact();


--
-- Name: delegation_update_outbox_event delegation_update_outbox_event_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER delegation_update_outbox_event_cannot_be_truncated BEFORE TRUNCATE ON delegation_update_outbox_event FOR EACH STATEMENT EXECUTE FUNCTION reject_outbox_table_truncate();


--
-- Name: delegation_update_outbox_event delegation_update_outbox_event_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER delegation_update_outbox_event_is_append_only BEFORE DELETE OR UPDATE ON delegation_update_outbox_event FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: delegation_update_outbox_event delegation_update_subject; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER delegation_update_subject AFTER INSERT ON delegation_update_outbox_event DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_delegation_update_subject();


--
-- Name: delegation_wake_outbox_event delegation_wake_outbox_event_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER delegation_wake_outbox_event_cannot_be_truncated BEFORE TRUNCATE ON delegation_wake_outbox_event FOR EACH STATEMENT EXECUTE FUNCTION reject_outbox_table_truncate();


--
-- Name: delegation_wake_outbox_event delegation_wake_outbox_event_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER delegation_wake_outbox_event_is_append_only BEFORE DELETE OR UPDATE ON delegation_wake_outbox_event FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: delegation_wake_outbox_event delegation_wake_recipient; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER delegation_wake_recipient AFTER INSERT ON delegation_wake_outbox_event DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_delegation_wake_recipient();


--
-- Name: session_delegation_initial_task initial_task_requires_relation_history; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER initial_task_requires_relation_history AFTER INSERT OR DELETE OR UPDATE ON session_delegation_initial_task DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_initial_task_relation_history();


--
-- Name: semantic_transcript_entry semantic_delegation_result_delivery_mode; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER semantic_delegation_result_delivery_mode AFTER INSERT ON semantic_transcript_entry DEFERRABLE INITIALLY DEFERRED FOR EACH ROW WHEN ((new.payload_kind = 'delegation_result'::text)) EXECUTE FUNCTION require_semantic_delegation_result_delivery_mode();


--
-- Name: session_child_result session_child_result_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_child_result_cannot_be_truncated BEFORE TRUNCATE ON session_child_result FOR EACH STATEMENT EXECUTE FUNCTION reject_session_delegation_table_truncate();


--
-- Name: session_child_result_delivery session_child_result_delivery_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_child_result_delivery_cannot_be_truncated BEFORE TRUNCATE ON session_child_result_delivery FOR EACH STATEMENT EXECUTE FUNCTION reject_session_delegation_table_truncate();


--
-- Name: session_child_result_delivery session_child_result_delivery_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_child_result_delivery_is_append_only BEFORE DELETE OR UPDATE ON session_child_result_delivery FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: session_child_result_delivery session_child_result_delivery_mode; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER session_child_result_delivery_mode AFTER INSERT ON session_child_result_delivery DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_session_child_result_delivery_mode();


--
-- Name: session_child_result session_child_result_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_child_result_is_append_only BEFORE DELETE OR UPDATE ON session_child_result FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: session_child_result session_child_result_zz_requires_update; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER session_child_result_zz_requires_update AFTER INSERT ON session_child_result DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_delegation_result_update();


--
-- Name: session_child_result session_child_result_zz_requires_wait_deliveries; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER session_child_result_zz_requires_wait_deliveries AFTER INSERT ON session_child_result DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_session_child_result_wait_deliveries();


--
-- Name: session_child_result session_child_result_zz_requires_wake; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER session_child_result_zz_requires_wake AFTER INSERT ON session_child_result DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_delegation_result_wake();


--
-- Name: session_delegation session_delegation_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_delegation_cannot_be_truncated BEFORE TRUNCATE ON session_delegation FOR EACH STATEMENT EXECUTE FUNCTION reject_session_delegation_table_truncate();


--
-- Name: session_delegation_termination_cascade session_delegation_cascade_requires_parent_chains; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER session_delegation_cascade_requires_parent_chains AFTER INSERT ON session_delegation_termination_cascade DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_delegation_cascade_parent_termination_chains();


--
-- Name: session_delegation_event session_delegation_event_append_guard; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_delegation_event_append_guard BEFORE INSERT ON session_delegation_event FOR EACH ROW EXECUTE FUNCTION guard_session_delegation_event_append();


--
-- Name: session_delegation_event session_delegation_event_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_delegation_event_cannot_be_truncated BEFORE TRUNCATE ON session_delegation_event FOR EACH STATEMENT EXECUTE FUNCTION reject_session_delegation_table_truncate();


--
-- Name: session_delegation_event session_delegation_event_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_delegation_event_is_append_only BEFORE DELETE OR UPDATE ON session_delegation_event FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: session_delegation_event session_delegation_event_requires_payload; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER session_delegation_event_requires_payload AFTER INSERT ON session_delegation_event DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_session_delegation_event_payload();


--
-- Name: session_delegation_event session_delegation_event_zz_requires_lifecycle_update; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER session_delegation_event_zz_requires_lifecycle_update AFTER INSERT ON session_delegation_event DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_delegation_lifecycle_update();


--
-- Name: session_delegation_initial_task session_delegation_initial_task_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_delegation_initial_task_cannot_be_truncated BEFORE TRUNCATE ON session_delegation_initial_task FOR EACH STATEMENT EXECUTE FUNCTION reject_session_delegation_table_truncate();


--
-- Name: session_delegation_initial_task session_delegation_initial_task_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_delegation_initial_task_is_append_only BEFORE DELETE OR UPDATE ON session_delegation_initial_task FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: session_delegation_initial_task session_delegation_initial_task_purpose; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER session_delegation_initial_task_purpose AFTER INSERT ON session_delegation_initial_task DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_delegation_initial_task_purpose();


--
-- Name: session_delegation session_delegation_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_delegation_is_append_only BEFORE DELETE OR UPDATE ON session_delegation FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: session_delegation session_delegation_locks_parent_for_spawn; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_delegation_locks_parent_for_spawn BEFORE INSERT ON session_delegation FOR EACH ROW EXECUTE FUNCTION lock_delegation_parent_for_spawn();


--
-- Name: session_delegation_logical_terminal session_delegation_logical_terminal_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_delegation_logical_terminal_cannot_be_truncated BEFORE TRUNCATE ON session_delegation_logical_terminal FOR EACH STATEMENT EXECUTE FUNCTION reject_session_delegation_table_truncate();


--
-- Name: session_delegation_logical_terminal session_delegation_logical_terminal_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_delegation_logical_terminal_is_append_only BEFORE DELETE OR UPDATE ON session_delegation_logical_terminal FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: session_delegation_logical_terminal session_delegation_logical_terminal_requires_outcome; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER session_delegation_logical_terminal_requires_outcome AFTER INSERT ON session_delegation_logical_terminal DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_delegation_logical_terminal_outcome();


--
-- Name: session_delegation_message_rejection session_delegation_message_rejection_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_delegation_message_rejection_cannot_be_truncated BEFORE TRUNCATE ON session_delegation_message_rejection FOR EACH STATEMENT EXECUTE FUNCTION reject_session_delegation_table_truncate();


--
-- Name: session_delegation_message_rejection session_delegation_message_rejection_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_delegation_message_rejection_is_append_only BEFORE DELETE OR UPDATE ON session_delegation_message_rejection FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: session_delegation_message_rejection session_delegation_message_rejection_requires_attempt; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER session_delegation_message_rejection_requires_attempt AFTER INSERT ON session_delegation_message_rejection DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_delegation_message_rejection_attempt();


--
-- Name: session_delegation_parent_termination session_delegation_parent_termination_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_delegation_parent_termination_cannot_be_truncated BEFORE TRUNCATE ON session_delegation_parent_termination FOR EACH STATEMENT EXECUTE FUNCTION reject_session_delegation_table_truncate();


--
-- Name: session_delegation_parent_termination session_delegation_parent_termination_chain; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER session_delegation_parent_termination_chain AFTER INSERT ON session_delegation_parent_termination DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_delegation_parent_termination_chain();


--
-- Name: session_delegation_parent_termination session_delegation_parent_termination_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_delegation_parent_termination_is_append_only BEFORE DELETE OR UPDATE ON session_delegation_parent_termination FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: session_delegation session_delegation_requires_spawn_history; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER session_delegation_requires_spawn_history AFTER INSERT ON session_delegation DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_delegation_spawn_history();


--
-- Name: session_delegation_termination_cascade session_delegation_termination_cascade_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_delegation_termination_cascade_cannot_be_truncated BEFORE TRUNCATE ON session_delegation_termination_cascade FOR EACH STATEMENT EXECUTE FUNCTION reject_session_delegation_table_truncate();


--
-- Name: session_delegation_termination_cascade session_delegation_termination_cascade_command; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER session_delegation_termination_cascade_command AFTER INSERT ON session_delegation_termination_cascade DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_delegation_termination_cascade_command();


--
-- Name: session_delegation_termination_cascade session_delegation_termination_cascade_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_delegation_termination_cascade_is_append_only BEFORE DELETE OR UPDATE ON session_delegation_termination_cascade FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: session_delegation_wait session_delegation_wait_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_delegation_wait_cannot_be_truncated BEFORE TRUNCATE ON session_delegation_wait FOR EACH STATEMENT EXECUTE FUNCTION reject_session_delegation_table_truncate();


--
-- Name: session_delegation_wait session_delegation_wait_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_delegation_wait_is_append_only BEFORE DELETE OR UPDATE ON session_delegation_wait FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: session_delegation_wait session_delegation_wait_purpose; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER session_delegation_wait_purpose AFTER INSERT ON session_delegation_wait DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_delegation_wait_purpose();


--
-- Name: session_delegation_wait_rejection session_delegation_wait_rejection_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_delegation_wait_rejection_cannot_be_truncated BEFORE TRUNCATE ON session_delegation_wait_rejection FOR EACH STATEMENT EXECUTE FUNCTION reject_session_delegation_table_truncate();


--
-- Name: session_delegation_wait_rejection session_delegation_wait_rejection_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_delegation_wait_rejection_is_append_only BEFORE DELETE OR UPDATE ON session_delegation_wait_rejection FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: session_delegation_wait_rejection session_delegation_wait_rejection_requires_attempt; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER session_delegation_wait_rejection_requires_attempt AFTER INSERT ON session_delegation_wait_rejection DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_delegation_wait_rejection_attempt();


--
-- Name: session_delegation_wait session_delegation_wait_requires_turn_phase; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER session_delegation_wait_requires_turn_phase AFTER INSERT ON session_delegation_wait DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_delegation_wait_turn_phase();


--
-- Name: session_delegation_wait session_delegation_wait_zz_requires_result_delivery; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER session_delegation_wait_zz_requires_result_delivery AFTER INSERT ON session_delegation_wait DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_session_child_result_wait_deliveries();


--
-- Name: session_delegation_wait session_delegation_wait_zz_requires_update; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER session_delegation_wait_zz_requires_update AFTER INSERT ON session_delegation_wait DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_delegation_wait_update();


--
-- Name: session_delegation_wake_turn_origin session_delegation_wake_turn_origin_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_delegation_wake_turn_origin_cannot_be_truncated BEFORE TRUNCATE ON session_delegation_wake_turn_origin FOR EACH STATEMENT EXECUTE FUNCTION reject_session_delegation_table_truncate();


--
-- Name: session_delegation_wake_turn_origin session_delegation_wake_turn_origin_changes_are_guarded; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_delegation_wake_turn_origin_changes_are_guarded BEFORE DELETE OR UPDATE ON session_delegation_wake_turn_origin FOR EACH ROW EXECUTE FUNCTION guard_session_delegation_wake_turn_origin_change();


--
-- Name: session_delegation_wake_turn_origin session_delegation_wake_turn_origin_is_valid; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER session_delegation_wake_turn_origin_is_valid AFTER INSERT OR UPDATE ON session_delegation_wake_turn_origin DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_delegation_wake_turn_origin();


--
-- Name: session_delegation session_delegation_zz_requires_spawn_update; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER session_delegation_zz_requires_spawn_update AFTER INSERT ON session_delegation DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_delegation_spawn_update();


--
-- Name: session_message session_message_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_message_cannot_be_truncated BEFORE TRUNCATE ON session_message FOR EACH STATEMENT EXECUTE FUNCTION reject_session_delegation_table_truncate();


--
-- Name: session_message_delivery session_message_delivery_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_message_delivery_cannot_be_truncated BEFORE TRUNCATE ON session_message_delivery FOR EACH STATEMENT EXECUTE FUNCTION reject_session_delegation_table_truncate();


--
-- Name: session_message_delivery session_message_delivery_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_message_delivery_is_append_only BEFORE DELETE OR UPDATE ON session_message_delivery FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: session_message_delivery session_message_delivery_recipient; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER session_message_delivery_recipient AFTER INSERT ON session_message_delivery DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_session_message_delivery_recipient();


--
-- Name: session_message session_message_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_message_is_append_only BEFORE DELETE OR UPDATE ON session_message FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: session_message session_message_requires_delivery; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER session_message_requires_delivery AFTER INSERT ON session_message DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_session_message_delivery();


--
-- Name: session_message session_message_zz_requires_update; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER session_message_zz_requires_update AFTER INSERT ON session_message DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_delegation_message_update();


--
-- Name: session_message session_message_zz_requires_wake; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER session_message_zz_requires_wake AFTER INSERT ON session_message DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_delegation_message_wake();


--
-- Name: session_pending_delivery session_pending_delivery_append_guard; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_pending_delivery_append_guard BEFORE INSERT ON session_pending_delivery FOR EACH ROW EXECUTE FUNCTION guard_session_pending_delivery_append();


--
-- Name: session_pending_delivery session_pending_delivery_cannot_be_truncated; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_pending_delivery_cannot_be_truncated BEFORE TRUNCATE ON session_pending_delivery FOR EACH STATEMENT EXECUTE FUNCTION reject_session_delegation_table_truncate();


--
-- Name: session_pending_delivery session_pending_delivery_is_append_only; Type: TRIGGER; Schema: public
--

CREATE TRIGGER session_pending_delivery_is_append_only BEFORE DELETE OR UPDATE ON session_pending_delivery FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();


--
-- Name: session_pending_delivery session_pending_delivery_requires_satellite; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER session_pending_delivery_requires_satellite AFTER INSERT ON session_pending_delivery DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_session_pending_delivery_satellite();


--
-- Name: submit_input_command submit_input_command_locks_delegation_frontier; Type: TRIGGER; Schema: public
--

CREATE TRIGGER submit_input_command_locks_delegation_frontier BEFORE INSERT ON submit_input_command FOR EACH ROW EXECUTE FUNCTION lock_delegation_frontier_before_input_interrupt();


--
-- Name: turn_lifecycle turn_lifecycle_requires_typed_origin; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER turn_lifecycle_requires_typed_origin AFTER INSERT OR UPDATE ON turn_lifecycle DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_delegation_initial_task_origin();


--
-- Name: turn_lifecycle turn_lifecycle_requires_valid_delegation_wake_origin; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER turn_lifecycle_requires_valid_delegation_wake_origin AFTER INSERT OR UPDATE ON turn_lifecycle DEFERRABLE INITIALLY DEFERRED FOR EACH ROW WHEN ((new.origin_kind = 'delegation'::text)) EXECUTE FUNCTION require_delegation_wake_turn_origin();


--
-- Name: turn_lifecycle turn_lifecycle_zz_requires_delegated_result; Type: TRIGGER; Schema: public
--

CREATE CONSTRAINT TRIGGER turn_lifecycle_zz_requires_delegated_result AFTER INSERT OR DELETE OR UPDATE ON turn_lifecycle DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION require_terminal_delegated_turn_result();


--
-- Foreign keys.
--

--
-- Name: delegation_outbox_event delegation_outbox_event_session_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY delegation_outbox_event
    ADD CONSTRAINT delegation_outbox_event_session_id_fkey FOREIGN KEY (session_id) REFERENCES session(session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: delegation_update_outbox_event delegation_update_outbox_even_awaiting_tool_request_id_spa_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY delegation_update_outbox_event
    ADD CONSTRAINT delegation_update_outbox_even_awaiting_tool_request_id_spa_fkey FOREIGN KEY (awaiting_tool_request_id, spawning_tool_request_id) REFERENCES session_delegation_wait(awaiting_tool_request_id, spawning_tool_request_id) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: delegation_update_outbox_event delegation_update_outbox_even_event_sequence_event_kind_st_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY delegation_update_outbox_event
    ADD CONSTRAINT delegation_update_outbox_even_event_sequence_event_kind_st_fkey FOREIGN KEY (event_sequence, event_kind, storage_version, session_id) REFERENCES delegation_outbox_event(event_sequence, event_kind, storage_version, session_id) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: delegation_update_outbox_event delegation_update_outbox_even_message_id_spawning_tool_req_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY delegation_update_outbox_event
    ADD CONSTRAINT delegation_update_outbox_even_message_id_spawning_tool_req_fkey FOREIGN KEY (message_id, spawning_tool_request_id) REFERENCES session_message(message_id, spawning_tool_request_id) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: delegation_update_outbox_event delegation_update_outbox_even_spawning_tool_request_id_chi_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY delegation_update_outbox_event
    ADD CONSTRAINT delegation_update_outbox_even_spawning_tool_request_id_chi_fkey FOREIGN KEY (spawning_tool_request_id, child_session_id) REFERENCES session_delegation(spawning_tool_request_id, child_session_id) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: delegation_update_outbox_event delegation_update_outbox_even_spawning_tool_request_id_del_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY delegation_update_outbox_event
    ADD CONSTRAINT delegation_update_outbox_even_spawning_tool_request_id_del_fkey FOREIGN KEY (spawning_tool_request_id, delegation_event_ordinal, delegation_event_kind) REFERENCES session_delegation_event(spawning_tool_request_id, event_ordinal, event_kind) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: delegation_update_outbox_event delegation_update_outbox_event_provenance_command_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY delegation_update_outbox_event
    ADD CONSTRAINT delegation_update_outbox_event_provenance_command_id_fkey FOREIGN KEY (provenance_command_id) REFERENCES durable_command(command_id) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: delegation_update_outbox_event delegation_update_outbox_event_recipient_session_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY delegation_update_outbox_event
    ADD CONSTRAINT delegation_update_outbox_event_recipient_session_id_fkey FOREIGN KEY (recipient_session_id) REFERENCES session(session_id) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: delegation_update_outbox_event delegation_update_outbox_event_result_spawning_request_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY delegation_update_outbox_event
    ADD CONSTRAINT delegation_update_outbox_event_result_spawning_request_id_fkey FOREIGN KEY (result_spawning_request_id) REFERENCES session_child_result(spawning_tool_request_id) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: delegation_update_outbox_event delegation_update_outbox_event_sender_session_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY delegation_update_outbox_event
    ADD CONSTRAINT delegation_update_outbox_event_sender_session_id_fkey FOREIGN KEY (sender_session_id) REFERENCES session(session_id) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: delegation_update_outbox_event delegation_update_outbox_event_spawning_tool_request_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY delegation_update_outbox_event
    ADD CONSTRAINT delegation_update_outbox_event_spawning_tool_request_id_fkey FOREIGN KEY (spawning_tool_request_id) REFERENCES session_delegation(spawning_tool_request_id) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: delegation_wake_outbox_event delegation_wake_outbox_event_awaiting_tool_request_id_resu_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY delegation_wake_outbox_event
    ADD CONSTRAINT delegation_wake_outbox_event_awaiting_tool_request_id_resu_fkey FOREIGN KEY (awaiting_tool_request_id, result_spawning_request_id, session_id) REFERENCES session_delegation_wait(awaiting_tool_request_id, spawning_tool_request_id, parent_session_id) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: delegation_wake_outbox_event delegation_wake_outbox_event_event_sequence_event_kind_sto_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY delegation_wake_outbox_event
    ADD CONSTRAINT delegation_wake_outbox_event_event_sequence_event_kind_sto_fkey FOREIGN KEY (event_sequence, event_kind, storage_version, session_id) REFERENCES delegation_outbox_event(event_sequence, event_kind, storage_version, session_id) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: delegation_wake_outbox_event delegation_wake_outbox_event_message_id_spawning_tool_requ_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY delegation_wake_outbox_event
    ADD CONSTRAINT delegation_wake_outbox_event_message_id_spawning_tool_requ_fkey FOREIGN KEY (message_id, spawning_tool_request_id) REFERENCES session_message(message_id, spawning_tool_request_id) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: delegation_wake_outbox_event delegation_wake_outbox_event_result_spawning_request_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY delegation_wake_outbox_event
    ADD CONSTRAINT delegation_wake_outbox_event_result_spawning_request_id_fkey FOREIGN KEY (result_spawning_request_id) REFERENCES session_child_result(spawning_tool_request_id) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: delegation_wake_outbox_event delegation_wake_outbox_event_result_spawning_request_id_se_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY delegation_wake_outbox_event
    ADD CONSTRAINT delegation_wake_outbox_event_result_spawning_request_id_se_fkey FOREIGN KEY (result_spawning_request_id, session_id) REFERENCES session_delegation(spawning_tool_request_id, parent_session_id) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: session_child_result_delivery session_child_result_delivery_awaiting_tool_request_id_spa_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_child_result_delivery
    ADD CONSTRAINT session_child_result_delivery_awaiting_tool_request_id_spa_fkey FOREIGN KEY (awaiting_tool_request_id, spawning_tool_request_id, parent_session_id) REFERENCES session_delegation_wait(awaiting_tool_request_id, spawning_tool_request_id, parent_session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: session_child_result_delivery session_child_result_delivery_parent_session_id_delivery_s_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_child_result_delivery
    ADD CONSTRAINT session_child_result_delivery_parent_session_id_delivery_s_fkey FOREIGN KEY (parent_session_id, delivery_sequence, delivery_kind) REFERENCES session_pending_delivery(recipient_session_id, delivery_sequence, delivery_kind) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: session_child_result_delivery session_child_result_delivery_spawning_tool_request_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_child_result_delivery
    ADD CONSTRAINT session_child_result_delivery_spawning_tool_request_id_fkey FOREIGN KEY (spawning_tool_request_id) REFERENCES session_child_result(spawning_tool_request_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: session_child_result session_child_result_spawning_tool_request_id_event_ordina_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_child_result
    ADD CONSTRAINT session_child_result_spawning_tool_request_id_event_ordina_fkey FOREIGN KEY (spawning_tool_request_id, event_ordinal, event_kind, outcome_kind) REFERENCES session_delegation_event(spawning_tool_request_id, event_ordinal, event_kind, outcome_kind) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: session_delegation session_delegation_child_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation
    ADD CONSTRAINT session_delegation_child_fk FOREIGN KEY (spawning_tool_request_id, child_session_id) REFERENCES session(spawning_tool_request_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: session_delegation_event session_delegation_event_provenance_command_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation_event
    ADD CONSTRAINT session_delegation_event_provenance_command_id_fkey FOREIGN KEY (provenance_command_id) REFERENCES durable_command(command_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: session_delegation_event session_delegation_event_provenance_tool_request_id_proven_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation_event
    ADD CONSTRAINT session_delegation_event_provenance_tool_request_id_proven_fkey FOREIGN KEY (provenance_tool_request_id, provenance_turn_id, provenance_session_id) REFERENCES tool_request(request_id, turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: session_delegation_event session_delegation_event_provenance_turn_id_provenance_ses_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation_event
    ADD CONSTRAINT session_delegation_event_provenance_turn_id_provenance_ses_fkey FOREIGN KEY (provenance_turn_id, provenance_session_id) REFERENCES turn_lifecycle(turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: session_delegation_event session_delegation_event_spawning_tool_request_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation_event
    ADD CONSTRAINT session_delegation_event_spawning_tool_request_id_fkey FOREIGN KEY (spawning_tool_request_id) REFERENCES session_delegation(spawning_tool_request_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: session_delegation_initial_task session_delegation_initial_ta_child_session_id_defaults_ve_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation_initial_task
    ADD CONSTRAINT session_delegation_initial_ta_child_session_id_defaults_ve_fkey FOREIGN KEY (child_session_id, defaults_version) REFERENCES session_defaults_version(session_id, version) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: session_delegation_initial_task session_delegation_initial_ta_spawning_tool_request_id_chi_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation_initial_task
    ADD CONSTRAINT session_delegation_initial_ta_spawning_tool_request_id_chi_fkey FOREIGN KEY (spawning_tool_request_id, child_session_id) REFERENCES session_delegation(spawning_tool_request_id, child_session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: session_delegation_initial_task session_delegation_initial_ta_turn_id_child_session_id_adm_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation_initial_task
    ADD CONSTRAINT session_delegation_initial_ta_turn_id_child_session_id_adm_fkey FOREIGN KEY (turn_id, child_session_id, admission_position) REFERENCES turn_lifecycle(turn_id, session_id, acceptance_position) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: session_delegation_initial_task session_delegation_initial_task_semantic_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation_initial_task
    ADD CONSTRAINT session_delegation_initial_task_semantic_fk FOREIGN KEY (spawning_tool_request_id, child_session_id, semantic_entry_id) REFERENCES semantic_transcript_entry(delegated_task_spawning_tool_request_id, source_session_id, semantic_entry_id) ON UPDATE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: session_delegation_logical_terminal session_delegation_logical_te_child_session_id_terminal_fr_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation_logical_terminal
    ADD CONSTRAINT session_delegation_logical_te_child_session_id_terminal_fr_fkey FOREIGN KEY (child_session_id, terminal_frontier_id) REFERENCES context_frontier(owning_session_id, context_frontier_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: session_delegation_logical_terminal session_delegation_logical_te_child_turn_id_child_session__fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation_logical_terminal
    ADD CONSTRAINT session_delegation_logical_te_child_turn_id_child_session__fkey FOREIGN KEY (child_turn_id, child_session_id) REFERENCES turn_lifecycle(turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: session_delegation_logical_terminal session_delegation_logical_te_spawning_tool_request_id_chi_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation_logical_terminal
    ADD CONSTRAINT session_delegation_logical_te_spawning_tool_request_id_chi_fkey FOREIGN KEY (spawning_tool_request_id, child_session_id, child_turn_id) REFERENCES session_delegation_initial_task(spawning_tool_request_id, child_session_id, turn_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: session_delegation_logical_terminal session_delegation_logical_te_spawning_tool_request_id_roo_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation_logical_terminal
    ADD CONSTRAINT session_delegation_logical_te_spawning_tool_request_id_roo_fkey FOREIGN KEY (spawning_tool_request_id, root_command_id) REFERENCES session_delegation_parent_termination(spawning_tool_request_id, root_command_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: session_delegation_message_rejection session_delegation_message_reject_spawning_tool_request_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation_message_rejection
    ADD CONSTRAINT session_delegation_message_reject_spawning_tool_request_id_fkey FOREIGN KEY (spawning_tool_request_id) REFERENCES session_delegation(spawning_tool_request_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: session_delegation_message_rejection session_delegation_message_rejection_tool_request_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation_message_rejection
    ADD CONSTRAINT session_delegation_message_rejection_tool_request_id_fkey FOREIGN KEY (tool_request_id) REFERENCES tool_request(request_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: session_delegation session_delegation_parent_request_fk; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation
    ADD CONSTRAINT session_delegation_parent_request_fk FOREIGN KEY (spawning_tool_request_id, parent_turn_id, parent_session_id) REFERENCES tool_request(request_id, turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: session_delegation_parent_termination session_delegation_parent_ter_parent_turn_id_parent_sessio_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation_parent_termination
    ADD CONSTRAINT session_delegation_parent_ter_parent_turn_id_parent_sessio_fkey FOREIGN KEY (parent_turn_id, parent_session_id) REFERENCES turn_lifecycle(turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: session_delegation_parent_termination session_delegation_parent_ter_source_spawning_tool_request_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation_parent_termination
    ADD CONSTRAINT session_delegation_parent_ter_source_spawning_tool_request_fkey FOREIGN KEY (source_spawning_tool_request_id, root_command_id) REFERENCES session_delegation_parent_termination(spawning_tool_request_id, root_command_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: session_delegation_parent_termination session_delegation_parent_ter_spawning_tool_request_id_par_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation_parent_termination
    ADD CONSTRAINT session_delegation_parent_ter_spawning_tool_request_id_par_fkey FOREIGN KEY (spawning_tool_request_id, parent_session_id) REFERENCES session_delegation(spawning_tool_request_id, parent_session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: session_delegation_parent_termination session_delegation_parent_termination_root_command_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation_parent_termination
    ADD CONSTRAINT session_delegation_parent_termination_root_command_id_fkey FOREIGN KEY (root_command_id) REFERENCES session_delegation_termination_cascade(root_command_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: session_delegation_termination_cascade session_delegation_terminatio_root_turn_id_root_session_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation_termination_cascade
    ADD CONSTRAINT session_delegation_terminatio_root_turn_id_root_session_id_fkey FOREIGN KEY (root_turn_id, root_session_id) REFERENCES turn_lifecycle(turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: session_delegation_termination_cascade session_delegation_termination_cascade_root_command_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation_termination_cascade
    ADD CONSTRAINT session_delegation_termination_cascade_root_command_id_fkey FOREIGN KEY (root_command_id) REFERENCES durable_command(command_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: session_delegation_wait session_delegation_wait_awaiting_tool_request_id_parent_tu_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation_wait
    ADD CONSTRAINT session_delegation_wait_awaiting_tool_request_id_parent_tu_fkey FOREIGN KEY (awaiting_tool_request_id, parent_turn_id, parent_session_id) REFERENCES tool_request(request_id, turn_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: session_delegation_wait_rejection session_delegation_wait_rejection_spawning_tool_request_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation_wait_rejection
    ADD CONSTRAINT session_delegation_wait_rejection_spawning_tool_request_id_fkey FOREIGN KEY (spawning_tool_request_id) REFERENCES session_delegation(spawning_tool_request_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: session_delegation_wait_rejection session_delegation_wait_rejection_tool_request_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation_wait_rejection
    ADD CONSTRAINT session_delegation_wait_rejection_tool_request_id_fkey FOREIGN KEY (tool_request_id) REFERENCES tool_request(request_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: session_delegation_wait session_delegation_wait_spawning_tool_request_id_parent_se_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation_wait
    ADD CONSTRAINT session_delegation_wait_spawning_tool_request_id_parent_se_fkey FOREIGN KEY (spawning_tool_request_id, parent_session_id, child_session_id) REFERENCES session_delegation(spawning_tool_request_id, parent_session_id, child_session_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: session_delegation_wake_turn_origin session_delegation_wake_turn__recipient_session_id_default_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation_wake_turn_origin
    ADD CONSTRAINT session_delegation_wake_turn__recipient_session_id_default_fkey FOREIGN KEY (recipient_session_id, defaults_version) REFERENCES session_defaults_version(session_id, version) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Name: session_delegation_wake_turn_origin session_delegation_wake_turn__recipient_session_id_through_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation_wake_turn_origin
    ADD CONSTRAINT session_delegation_wake_turn__recipient_session_id_through_fkey FOREIGN KEY (recipient_session_id, through_delivery_sequence) REFERENCES session_pending_delivery(recipient_session_id, delivery_sequence) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: session_delegation_wake_turn_origin session_delegation_wake_turn__turn_id_recipient_session_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_delegation_wake_turn_origin
    ADD CONSTRAINT session_delegation_wake_turn__turn_id_recipient_session_id_fkey FOREIGN KEY (turn_id, recipient_session_id, admission_position) REFERENCES turn_lifecycle(turn_id, session_id, acceptance_position) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: session_message_delivery session_message_delivery_message_id_spawning_tool_request__fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_message_delivery
    ADD CONSTRAINT session_message_delivery_message_id_spawning_tool_request__fkey FOREIGN KEY (message_id, spawning_tool_request_id) REFERENCES session_message(message_id, spawning_tool_request_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: session_message_delivery session_message_delivery_recipient_session_id_delivery_seq_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_message_delivery
    ADD CONSTRAINT session_message_delivery_recipient_session_id_delivery_seq_fkey FOREIGN KEY (recipient_session_id, delivery_sequence, delivery_kind) REFERENCES session_pending_delivery(recipient_session_id, delivery_sequence, delivery_kind) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: session_message session_message_spawning_tool_request_id_event_ordinal_eve_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_message
    ADD CONSTRAINT session_message_spawning_tool_request_id_event_ordinal_eve_fkey FOREIGN KEY (spawning_tool_request_id, event_ordinal, event_kind) REFERENCES session_delegation_event(spawning_tool_request_id, event_ordinal, event_kind) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED;


--
-- Name: session_pending_delivery session_pending_delivery_recipient_session_id_fkey; Type: FK CONSTRAINT; Schema: public
--

ALTER TABLE ONLY session_pending_delivery
    ADD CONSTRAINT session_pending_delivery_recipient_session_id_fkey FOREIGN KEY (recipient_session_id) REFERENCES session(session_id) ON UPDATE RESTRICT ON DELETE RESTRICT;


--
-- Search-path pins for this file's constraint-reachable functions.
--
-- The pin has to name the schema the migration selected rather than a
-- literal, so it is applied here through current_schema instead of inline
-- in each CREATE FUNCTION (the full rationale is in 202609010000_core.sql;
-- crates/persistence/tests/search_path_postgres.rs is the guard).
--

DO $search_path_pins$
DECLARE
    signature text;
BEGIN
    -- the canonical restore-safe pin: the migration-selected schema, then pg_catalog, then pg_temp
    FOREACH signature IN ARRAY ARRAY[
        'require_session_delegation_event_payload()',
        'require_terminal_delegated_turn_result()'
    ] LOOP
        EXECUTE format('ALTER FUNCTION %s SET search_path TO %I, pg_catalog, pg_temp',
                   signature, current_schema);
    END LOOP;
END
$search_path_pins$;


--
-- Restore body validation for anything applied after this file.
--

RESET check_function_bodies;
