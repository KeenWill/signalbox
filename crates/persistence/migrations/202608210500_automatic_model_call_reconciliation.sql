-- Durable, bounded daemon reconciliation of ambiguous model calls.

CREATE TABLE automatic_model_call_reconciliation (
    turn_id uuid PRIMARY KEY,
    session_id uuid NOT NULL,
    model_call_id uuid NOT NULL,
    state_kind text NOT NULL DEFAULT 'scheduled',
    attempt_count integer NOT NULL DEFAULT 0,
    next_attempt_at timestamptz NOT NULL DEFAULT statement_timestamp(),
    exhausted_at timestamptz,
    FOREIGN KEY (turn_id, session_id)
        REFERENCES turn_lifecycle (turn_id, session_id),
    FOREIGN KEY (model_call_id, turn_id, session_id)
        REFERENCES model_call (model_call_id, turn_id, session_id),
    CONSTRAINT automatic_model_call_reconciliation_state_kind
        CHECK (state_kind IN ('scheduled', 'attempting', 'reconciled', 'superseded', 'exhausted')),
    CONSTRAINT automatic_model_call_reconciliation_attempt_count
        CHECK (attempt_count BETWEEN 0 AND 5),
    CONSTRAINT automatic_model_call_reconciliation_exhaustion
        CHECK ((state_kind = 'exhausted') = (exhausted_at IS NOT NULL))
);

CREATE INDEX automatic_model_call_reconciliation_due
    ON automatic_model_call_reconciliation (next_attempt_at, turn_id)
    WHERE state_kind IN ('scheduled', 'attempting');

CREATE TABLE automatic_model_call_reconciliation_discovery_state (
    singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
    after_turn_id uuid,
    high_turn_id uuid
);

INSERT INTO automatic_model_call_reconciliation_discovery_state (singleton)
VALUES (true);

CREATE TABLE automatic_model_call_reconciliation_supersession_state (
    singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
    after_turn_id uuid
);

INSERT INTO automatic_model_call_reconciliation_supersession_state (singleton)
VALUES (true);

CREATE INDEX turn_lifecycle_automatic_model_call_recovery_discovery
    ON turn_lifecycle (turn_id)
    INCLUDE (session_id, recovery_model_call_id)
    WHERE state_kind = 'active'
      AND active_phase_kind = 'awaiting_model_call_recovery'
      AND NOT delegation_runtime_terminal
      AND recovery_model_call_id IS NOT NULL;

CREATE INDEX automatic_model_call_reconciliation_supersession
    ON automatic_model_call_reconciliation (turn_id)
    INCLUDE (session_id, model_call_id, state_kind, attempt_count)
    WHERE state_kind IN ('scheduled', 'attempting', 'exhausted');

CREATE TABLE automatic_model_call_reconciliation_attempt (
    turn_id uuid NOT NULL,
    attempt_ordinal integer NOT NULL,
    outcome_kind text NOT NULL DEFAULT 'attempting',
    started_at timestamptz NOT NULL DEFAULT statement_timestamp(),
    finished_at timestamptz,
    PRIMARY KEY (turn_id, attempt_ordinal),
    FOREIGN KEY (turn_id)
        REFERENCES automatic_model_call_reconciliation (turn_id),
    CONSTRAINT automatic_model_call_reconciliation_attempt_ordinal
        CHECK (attempt_ordinal BETWEEN 1 AND 5),
    CONSTRAINT automatic_model_call_reconciliation_attempt_outcome
        CHECK (outcome_kind IN (
            'attempting',
            'reconciled',
            'superseded',
            'infrastructure_failure',
            'integrity_failure'
        )),
    CONSTRAINT automatic_model_call_reconciliation_attempt_finished
        CHECK ((outcome_kind = 'attempting') = (finished_at IS NULL))
);

-- The existing reconciliation transition requires an applied interrupt.
-- Daemon-owned recovery supplies its typed, budgeted authority through the
-- exact reconciled state row instead; every other final-state proof remains
-- unchanged.
CREATE OR REPLACE FUNCTION assert_reconciliation_required_turn_final_state(
    checked_turn_id uuid
)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    checked_session uuid;
    checked_attempt uuid;
    checked_call uuid;
    checked_tool_attempt uuid;
    checked_terminal_frontier uuid;
    source_frontier uuid;
    source_tool_round uuid;
    source_frontier_count bigint;
    member_mismatch_count bigint;
    contradictory_entry_count bigint;
    outbox_count bigint;
    interrupt_command uuid;
    interrupt_record_count bigint;
BEGIN
    SELECT
        session_id,
        terminal_attempt_id,
        terminal_model_call_id,
        terminal_tool_attempt_id,
        terminal_frontier_id
      INTO
        checked_session,
        checked_attempt,
        checked_call,
        checked_tool_attempt,
        checked_terminal_frontier
      FROM turn_lifecycle
     WHERE turn_id = checked_turn_id
       AND state_kind = 'terminal'
       AND terminal_disposition_kind = 'reconciliation_required';

    IF NOT FOUND THEN
        RETURN;
    END IF;

    PERFORM assert_terminal_started_turn_common_final_state(checked_turn_id);

    IF NOT EXISTS (
        SELECT 1
          FROM turn_attempt
         WHERE turn_attempt_id = checked_attempt
           AND turn_id = checked_turn_id
           AND session_id = checked_session
           AND state_kind = 'ended'
           AND (
                end_disposition IN ('ambiguous', 'lost')
                OR (
                    end_disposition = 'yielded_to_durable_wait'
                    AND EXISTS (
                        SELECT 1
                          FROM turn_runner_recovery_interrupt_effect AS effect
                         WHERE effect.session_id = checked_session
                           AND effect.turn_id = checked_turn_id
                           AND effect.yielded_turn_attempt_id =
                                turn_attempt.turn_attempt_id
                           AND effect.interrupted_tool_attempt_id =
                                checked_tool_attempt
                    )
                )
           )
           AND (
                (
                    end_variant = 'after_cancellation'
                    AND interrupt_command_id IS NOT NULL
                    AND interrupt_predecessor_turn_id = checked_turn_id
                )
                OR (
                    end_variant = 'without_stop'
                    AND interrupt_command_id IS NULL
                    AND interrupt_predecessor_turn_id IS NULL
                )
           )
    ) THEN
        RAISE EXCEPTION
            'reconciliation-required turn lacks exact ambiguous attempt'
            USING ERRCODE = '23514';
    END IF;

    SELECT interrupt_command_id
      INTO interrupt_command
      FROM turn_attempt
     WHERE turn_attempt_id = checked_attempt;
    IF interrupt_command IS NOT NULL THEN
        PERFORM assert_interrupt_attempt_proof(checked_attempt);
    END IF;

    SELECT count(*)
      INTO interrupt_record_count
      FROM submit_input_command AS command
      JOIN accepted_input AS accepted
        ON accepted.accepting_command_id = command.command_id
       AND accepted.accepted_input_id = command.result_accepted_input_id
       AND accepted.session_id = command.result_session_id
       AND accepted.origin_turn_id = command.result_turn_id
      JOIN queued_input_origin AS successor
        ON successor.accepted_input_id = accepted.accepted_input_id
       AND successor.turn_id = accepted.origin_turn_id
       AND successor.session_id = accepted.session_id
       AND successor.priority_kind = 'interrupt_immediately_after'
       AND successor.interrupt_predecessor_turn_id = checked_turn_id
     WHERE command.session_id = checked_session
       AND command.delivery_kind = 'interrupt'
       AND command.expected_active_turn_id = checked_turn_id
       AND command.result_kind = 'applied'
       AND command.rejection_kind IS NULL
       AND accepted.disposition_kind = 'origin_of'
       AND (
            interrupt_command IS NULL
            OR command.command_id = interrupt_command
       );
    IF interrupt_record_count = 0
       AND NOT EXISTS (
            SELECT 1
              FROM automatic_model_call_reconciliation AS recovery
             WHERE recovery.turn_id = checked_turn_id
               AND recovery.session_id = checked_session
               AND recovery.model_call_id = checked_call
               AND recovery.state_kind = 'reconciled'
               AND recovery.attempt_count BETWEEN 1 AND 5
       )
    THEN
        RAISE EXCEPTION
            'reconciliation-required turn lacks exact automatic recovery authority'
            USING ERRCODE = '23514';
    END IF;
    IF interrupt_record_count > 1 THEN
        RAISE EXCEPTION
            'reconciliation-required turn has more than one applied interrupt'
            USING ERRCODE = '23514';
    END IF;

    IF checked_call IS NOT NULL THEN
        SELECT context_frontier_id
          INTO source_frontier
          FROM model_call
         WHERE model_call_id = checked_call
           AND turn_attempt_id = checked_attempt
           AND turn_id = checked_turn_id
           AND session_id = checked_session
           AND state_kind = 'terminal'
           AND terminal_disposition_kind = 'ambiguous';
        IF NOT FOUND THEN
            RAISE EXCEPTION
                'reconciliation-required turn lacks exact ambiguous call'
                USING ERRCODE = '23514';
        END IF;
        PERFORM assert_model_call_final_state(checked_call);
    ELSE
        SELECT
            round.boundary_frontier_id,
            round.producing_model_call_id
          INTO
            source_frontier,
            source_tool_round
          FROM tool_attempt AS attempt
          JOIN tool_request AS request
            ON request.request_id = attempt.request_id
          JOIN tool_round AS round
            ON round.producing_model_call_id =
               request.producing_model_call_id
           AND round.turn_id = request.turn_id
           AND round.session_id = request.session_id
         WHERE attempt.attempt_id = checked_tool_attempt
           AND attempt.issuing_turn_attempt_id = checked_attempt
           AND attempt.turn_id = checked_turn_id
           AND attempt.session_id = checked_session
           AND attempt.state_kind = 'terminal'
           AND attempt.terminal_disposition_kind = 'ambiguous'
           AND round.boundary_kind = 'continuing';
        IF NOT FOUND THEN
            RAISE EXCEPTION
                'reconciliation-required turn lacks exact ambiguous tool attempt'
                USING ERRCODE = '23514';
        END IF;
    END IF;

    IF checked_call IS NOT NULL THEN
        SELECT count(*)
          INTO member_mismatch_count
          FROM (
                (
                    SELECT
                        member_position,
                        source_session_id,
                        semantic_entry_id
                      FROM context_frontier_member
                     WHERE owning_session_id = checked_session
                       AND context_frontier_id = source_frontier
                    EXCEPT
                    SELECT
                        member_position,
                        source_session_id,
                        semantic_entry_id
                      FROM context_frontier_member
                     WHERE owning_session_id = checked_session
                       AND context_frontier_id =
                           checked_terminal_frontier
                )
                UNION ALL
                (
                    SELECT
                        member_position,
                        source_session_id,
                        semantic_entry_id
                      FROM context_frontier_member
                     WHERE owning_session_id = checked_session
                       AND context_frontier_id =
                           checked_terminal_frontier
                    EXCEPT
                    SELECT
                        member_position,
                        source_session_id,
                        semantic_entry_id
                      FROM context_frontier_member
                     WHERE owning_session_id = checked_session
                       AND context_frontier_id = source_frontier
                )
          ) AS mismatch;
    ELSE
        SELECT count(*)
          INTO source_frontier_count
          FROM context_frontier_member
         WHERE owning_session_id = checked_session
           AND context_frontier_id = source_frontier;

        WITH expected_member AS (
            SELECT
                member_position,
                source_session_id,
                semantic_entry_id
              FROM context_frontier_member
             WHERE owning_session_id = checked_session
               AND context_frontier_id = source_frontier
            UNION ALL
            SELECT
                source_frontier_count + request.request_ordinal + 1,
                result.source_session_id,
                result.semantic_entry_id
              FROM tool_request AS request
              JOIN semantic_transcript_entry AS result
                ON result.source_session_id = checked_session
               AND result.payload_kind IN (
                    'tool_execution_result',
                    'tool_denied',
                    'tool_closed_by_turn_end',
                    'delegation_result'
               )
              LEFT JOIN tool_attempt AS result_attempt
                ON result_attempt.attempt_id =
                   result.tool_result_attempt_id
             WHERE request.producing_model_call_id = source_tool_round
               AND (
                    result.tool_result_request_id = request.request_id
                    OR result_attempt.request_id = request.request_id
               )
        )
        SELECT count(*)
          INTO member_mismatch_count
          FROM (
                (
                    SELECT *
                      FROM expected_member
                    EXCEPT
                    SELECT
                        member_position,
                        source_session_id,
                        semantic_entry_id
                      FROM context_frontier_member
                     WHERE owning_session_id = checked_session
                       AND context_frontier_id =
                           checked_terminal_frontier
                )
                UNION ALL
                (
                    SELECT
                        member_position,
                        source_session_id,
                        semantic_entry_id
                      FROM context_frontier_member
                     WHERE owning_session_id = checked_session
                       AND context_frontier_id =
                           checked_terminal_frontier
                    EXCEPT
                    SELECT *
                      FROM expected_member
                )
          ) AS mismatch;
    END IF;

    SELECT count(*)
      INTO contradictory_entry_count
      FROM semantic_transcript_entry
     WHERE source_session_id = checked_session
       AND (
            failed_turn_id = checked_turn_id
            OR completed_turn_id = checked_turn_id
            OR cancelled_turn_id = checked_turn_id
            OR (
                checked_call IS NOT NULL
                AND producing_model_call_id = checked_call
            )
       )
       AND payload_kind IN (
            'turn_failed',
            'turn_completed',
            'turn_cancelled',
            'assistant_text'
       );

    SELECT count(*)
      INTO outbox_count
      FROM turn_reconciliation_required_outbox_event
     WHERE session_id = checked_session
       AND turn_id = checked_turn_id
       AND model_call_id IS NOT DISTINCT FROM checked_call
       AND tool_attempt_id IS NOT DISTINCT FROM checked_tool_attempt
       AND terminal_frontier_id = checked_terminal_frontier;

    IF member_mismatch_count <> 0
       OR contradictory_entry_count <> 0
       OR outbox_count <> 1
    THEN
        RAISE EXCEPTION
            'reconciliation-required turn lacks exact frontier or outbox boundary'
            USING ERRCODE = '23514';
    END IF;
END;
$$;

-- CREATE OR REPLACE clears per-function configuration. Restore the canonical
-- persistent-schema pin used by constraint-reachable functions.
DO $migration$
DECLARE
    migration_schema name := pg_catalog.current_schema();
BEGIN
    EXECUTE pg_catalog.format(
        'ALTER FUNCTION %I.assert_reconciliation_required_turn_final_state(uuid) '
        'SET search_path TO %I, pg_catalog, pg_temp',
        migration_schema,
        migration_schema
    );
END;
$migration$;

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
                                  FROM automatic_model_call_reconciliation AS recovery
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
          FROM automatic_model_call_reconciliation AS recovery
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
