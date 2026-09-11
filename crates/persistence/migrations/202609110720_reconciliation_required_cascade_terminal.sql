CREATE OR REPLACE FUNCTION delegation_cascade_expected_frontier(checked_root_session uuid, checked_root_kind text) RETURNS TABLE(spawning_tool_request_id uuid, parent_session_id uuid, child_session_id uuid, effective_parent_kind text, source_kind text, source_spawning_tool_request_id uuid, expected_action text)
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
                WHEN parent_result.spawning_tool_request_id IS NOT NULL
                    OR parent_lifecycle.turn_id IS NOT NULL THEN
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
                WHEN (
                        parent_result.spawning_tool_request_id IS NOT NULL
                        OR parent_lifecycle.turn_id IS NOT NULL
                     )
                    AND parent.effective_parent_kind = 'stopped'
                    THEN relation.on_parent_stopped
                WHEN parent_result.spawning_tool_request_id IS NOT NULL
                    OR parent_lifecycle.turn_id IS NOT NULL
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
          LEFT JOIN session_delegation_initial_task AS parent_task
            ON parent_task.spawning_tool_request_id =
                parent.spawning_tool_request_id
          LEFT JOIN turn_lifecycle AS parent_lifecycle
            ON parent_lifecycle.turn_id = parent_task.turn_id
           AND parent_lifecycle.session_id = parent_task.child_session_id
           AND parent_lifecycle.state_kind = 'terminal'
           AND parent_lifecycle.terminal_disposition_kind =
                'reconciliation_required'
         WHERE (
                parent.expected_action IN ('stop', 'cancel')
                OR parent_result.spawning_tool_request_id IS NOT NULL
                OR parent_lifecycle.turn_id IS NOT NULL
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

DO $migration$
DECLARE
    definition text;
    old_fragment text := $old$        IF EXISTS (
            SELECT 1 FROM session_child_result AS result
             WHERE result.spawning_tool_request_id =
                    frontier.spawning_tool_request_id
        ) THEN
$old$;
    new_fragment text := $new$        IF EXISTS (
            SELECT 1 FROM session_child_result AS result
             WHERE result.spawning_tool_request_id =
                    frontier.spawning_tool_request_id
        ) OR EXISTS (
            SELECT 1
              FROM session_delegation_initial_task AS task
              JOIN turn_lifecycle AS lifecycle
                ON lifecycle.turn_id = task.turn_id
               AND lifecycle.session_id = task.child_session_id
             WHERE task.spawning_tool_request_id =
                    frontier.spawning_tool_request_id
               AND lifecycle.state_kind = 'terminal'
               AND lifecycle.terminal_disposition_kind =
                    'reconciliation_required'
        ) THEN
$new$;
BEGIN
    definition := pg_get_functiondef(
        'materialize_session_delegation_termination_cascade(uuid, text)'::regprocedure
    );
    IF position(old_fragment IN definition) = 0 THEN
        RAISE EXCEPTION 'cascade terminal predicate was not found';
    END IF;
    EXECUTE replace(definition, old_fragment, new_fragment);
END;
$migration$;

DO $migration$
DECLARE
    definition text;
    old_fragment text := $old$            IF NEW.outcome_kind = 'already_terminal' AND NOT EXISTS (
                SELECT 1 FROM session_child_result AS prior
                 WHERE prior.spawning_tool_request_id = NEW.spawning_tool_request_id
                   AND prior.event_ordinal < NEW.event_ordinal
            ) THEN
$old$;
    new_fragment text := $new$            IF NEW.outcome_kind = 'already_terminal'
               AND NOT EXISTS (
                    SELECT 1 FROM session_child_result AS prior
                     WHERE prior.spawning_tool_request_id = NEW.spawning_tool_request_id
                       AND prior.event_ordinal < NEW.event_ordinal
               )
               AND NOT EXISTS (
                    SELECT 1
                      FROM session_delegation_initial_task AS task
                      JOIN turn_lifecycle AS lifecycle
                        ON lifecycle.turn_id = task.turn_id
                       AND lifecycle.session_id = task.child_session_id
                     WHERE task.spawning_tool_request_id =
                            NEW.spawning_tool_request_id
                       AND lifecycle.state_kind = 'terminal'
                       AND lifecycle.terminal_disposition_kind =
                            'reconciliation_required'
               ) THEN
$new$;
BEGIN
    definition := pg_get_functiondef(
        'require_session_delegation_event_payload()'::regprocedure
    );
    IF position(old_fragment IN definition) = 0 THEN
        RAISE EXCEPTION 'already-terminal evidence predicate was not found';
    END IF;
    EXECUTE replace(definition, old_fragment, new_fragment);
END;
$migration$;
