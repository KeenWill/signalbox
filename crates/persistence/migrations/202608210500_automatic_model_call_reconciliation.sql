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
    after_turn_id uuid
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
      AND origin_kind = 'accepted_input'
      AND active_phase_kind = 'awaiting_model_call_recovery'
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
