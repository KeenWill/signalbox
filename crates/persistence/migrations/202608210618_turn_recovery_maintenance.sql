-- Bound automatic operation reconciliation maintenance and preserve delegated recovery results.

CREATE TABLE automatic_reconciliation_discovery_state (
    singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
    after_turn_id uuid,
    high_turn_id uuid
);

INSERT INTO automatic_reconciliation_discovery_state (singleton)
VALUES (true);

-- Supersession laps carry a high-water mark for the same reason discovery laps
-- do: a recovery becomes superseded by a `turn_lifecycle` change rather than by
-- anything the supersession statement writes, so a row the cursor has already
-- passed can acquire that disposition afterwards. Without a mark, a steady
-- arrival rate keeps every page full, the cursor never wraps to NULL, and the
-- older rows starve.
CREATE TABLE automatic_reconciliation_supersession_state (
    singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
    after_turn_id uuid,
    high_turn_id uuid
);

INSERT INTO automatic_reconciliation_supersession_state (singleton)
VALUES (true);

CREATE INDEX turn_lifecycle_automatic_reconciliation_discovery
    ON turn_lifecycle (turn_id)
    INCLUDE (session_id, recovery_model_call_id, recovery_tool_attempt_id)
    WHERE state_kind = 'active'
      AND active_phase_kind IN (
          'awaiting_model_call_recovery',
          'awaiting_tool_recovery'
      )
      AND NOT delegation_runtime_terminal
      AND num_nonnulls(
          recovery_model_call_id,
          recovery_tool_attempt_id
      ) = 1;

CREATE INDEX automatic_reconciliation_supersession
    ON automatic_reconciliation (turn_id)
    INCLUDE (
        session_id,
        model_call_id,
        tool_attempt_id,
        state_kind,
        attempt_count
    )
    WHERE state_kind IN ('scheduled', 'attempting', 'exhausted');

CREATE OR REPLACE FUNCTION require_session_delegation_event_payload()
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

DO $migration$
DECLARE
    migration_schema name := pg_catalog.current_schema();
BEGIN
    EXECUTE pg_catalog.format(
        'ALTER FUNCTION %I.require_session_delegation_event_payload() '
        'SET search_path TO %I, pg_catalog, pg_temp',
        migration_schema,
        migration_schema
    );
END;
$migration$;
-- Automatic reconciliation closes delegated initial-task work with one typed
-- unavailable result. Direct operator reconciliation retains the historical
-- unresolved relationship shape.
CREATE OR REPLACE FUNCTION require_terminal_delegated_turn_result()
RETURNS trigger LANGUAGE plpgsql AS $$
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

DO $migration$
DECLARE
    migration_schema name := pg_catalog.current_schema();
BEGIN
    EXECUTE pg_catalog.format(
        'ALTER FUNCTION %I.require_terminal_delegated_turn_result() '
        'SET search_path TO %I, pg_catalog, pg_temp',
        migration_schema,
        migration_schema
    );
END;
$migration$;
