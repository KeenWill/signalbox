-- Audit unattended approval escalation closeout without making it a new
-- lifecycle authority. The linked failed turn and blocked goal remain the
-- ordinary authorities that release and re-arm repository-watch dispatch.

CREATE TABLE repo_watch_headless_approval_escalation (
    model_call_id uuid PRIMARY KEY,
    request_id uuid NOT NULL UNIQUE,
    dispatch_id uuid NOT NULL,
    action_ordinal integer NOT NULL,
    session_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    terminal_attempt_id uuid NOT NULL UNIQUE,
    failure_entry_id uuid NOT NULL UNIQUE,
    terminal_frontier_id uuid NOT NULL UNIQUE,
    escalated_at timestamptz NOT NULL DEFAULT transaction_timestamp(),

    FOREIGN KEY (model_call_id, session_id)
        REFERENCES tool_approval_judge_model_call (model_call_id, session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT,
    FOREIGN KEY (request_id, turn_id, session_id)
        REFERENCES tool_request (request_id, turn_id, session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT,
    FOREIGN KEY (dispatch_id, action_ordinal)
        REFERENCES repo_watch_dispatch_action (dispatch_id, action_ordinal)
        ON UPDATE RESTRICT ON DELETE RESTRICT,
    FOREIGN KEY (terminal_attempt_id, turn_id, session_id)
        REFERENCES turn_attempt (turn_attempt_id, turn_id, session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT,
    FOREIGN KEY (session_id, failure_entry_id)
        REFERENCES semantic_transcript_entry (source_session_id, semantic_entry_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT,
    FOREIGN KEY (session_id, terminal_frontier_id)
        REFERENCES context_frontier (owning_session_id, context_frontier_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT,
    FOREIGN KEY (turn_id, session_id)
        REFERENCES turn_lifecycle (turn_id, session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT
);

CREATE TRIGGER repo_watch_headless_approval_escalation_is_append_only
BEFORE UPDATE OR DELETE ON repo_watch_headless_approval_escalation
FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER repo_watch_headless_approval_escalation_reject_truncate
BEFORE TRUNCATE ON repo_watch_headless_approval_escalation
FOR EACH STATEMENT EXECUTE FUNCTION reject_repo_watch_table_truncate();

CREATE VIEW repo_watch_headless_approval_escalation_audit AS
SELECT escalation.model_call_id,
       escalation.request_id,
       escalation.dispatch_id,
       escalation.action_ordinal,
       escalation.session_id,
       escalation.turn_id,
       escalation.terminal_attempt_id,
       escalation.failure_entry_id,
       escalation.terminal_frontier_id,
       judge.rationale,
       escalation.escalated_at,
       released.released_at,
       owed.obligation_id,
       owed.settled_kind AS obligation_settled_kind,
       owed.settled_at AS obligation_settled_at
  FROM repo_watch_headless_approval_escalation AS escalation
  JOIN tool_approval_judge_model_call AS judge
    ON judge.model_call_id = escalation.model_call_id
  LEFT JOIN repo_watch_dispatch_release AS released
    ON released.dispatch_id = escalation.dispatch_id
  LEFT JOIN LATERAL (
       SELECT obligation.obligation_id,
              obligation.settled_kind,
              obligation.settled_at
         FROM repo_watch_dispatch_obligation AS obligation
        WHERE obligation.blocking_dispatch_id = escalation.dispatch_id
        ORDER BY obligation.owed_since DESC, obligation.obligation_id DESC
        LIMIT 1
  ) AS owed ON true;

-- A failed tool-loop turn ordinarily closes on a terminal model call or the
-- exact crash-lost tool attempt that made execution fail. A headless judge
-- escalation is a third, typed cause: the provider call completed, so neither
-- of those records would be truthful. Admit only the append-only escalation
-- record correlated to the terminal attempt and completed judge result.
CREATE OR REPLACE FUNCTION assert_failed_terminal_execution_without_cancellation(
    checked_turn_id uuid
)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    lifecycle turn_lifecycle%ROWTYPE;
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM tool_round
         WHERE turn_id = checked_turn_id
    ) THEN
        PERFORM assert_failed_terminal_execution_without_tool_loop(
            checked_turn_id
        );
        RETURN;
    END IF;

    SELECT *
      INTO lifecycle
      FROM turn_lifecycle
     WHERE turn_id = checked_turn_id
       AND state_kind = 'terminal'
       AND terminal_disposition_kind = 'failed';
    IF NOT FOUND THEN
        RETURN;
    END IF;

    IF lifecycle.terminal_attempt_id IS NULL
       OR NOT EXISTS (
            SELECT 1
              FROM turn_attempt
             WHERE turn_attempt_id = lifecycle.terminal_attempt_id
               AND turn_id = lifecycle.turn_id
               AND session_id = lifecycle.session_id
               AND state_kind = 'ended'
               AND end_variant = 'without_stop'
               AND end_disposition IN ('known_failure', 'lost')
       )
       OR EXISTS (
            SELECT 1
              FROM turn_attempt
             WHERE turn_id = lifecycle.turn_id
               AND session_id = lifecycle.session_id
               AND turn_attempt_id <> lifecycle.terminal_attempt_id
               AND (
                    state_kind <> 'ended'
                    OR end_variant <> 'without_stop'
                    OR end_disposition <> 'yielded_to_durable_wait'
               )
       )
    THEN
        RAISE EXCEPTION
            'failed tool-loop turn % lacks its exact linear ended attempt',
            checked_turn_id
            USING ERRCODE = '23514';
    END IF;

    IF lifecycle.terminal_model_call_id IS NOT NULL THEN
        IF NOT EXISTS (
            SELECT 1
              FROM model_call
             WHERE model_call_id = lifecycle.terminal_model_call_id
               AND turn_attempt_id = lifecycle.terminal_attempt_id
               AND turn_id = lifecycle.turn_id
               AND session_id = lifecycle.session_id
               AND state_kind = 'terminal'
               AND terminal_disposition_kind IN ('known_failed', 'cancelled')
        ) THEN
            RAISE EXCEPTION
                'failed tool-loop turn % lacks its exact terminal call',
                checked_turn_id
                USING ERRCODE = '23514';
        END IF;
        PERFORM assert_model_call_final_state(
            lifecycle.terminal_model_call_id
        );
    ELSIF NOT EXISTS (
        SELECT 1
          FROM tool_attempt
         WHERE issuing_turn_attempt_id = lifecycle.terminal_attempt_id
           AND turn_id = lifecycle.turn_id
           AND session_id = lifecycle.session_id
           AND state_kind = 'terminal'
           AND terminal_disposition_kind = 'known_failed'
           AND error_kind = 'crash_lost'
    ) AND NOT EXISTS (
        SELECT 1
          FROM repo_watch_headless_approval_escalation AS escalation
          JOIN tool_approval_judge_model_call AS judge
            ON judge.model_call_id = escalation.model_call_id
           AND judge.session_id = escalation.session_id
           AND judge.turn_id = escalation.turn_id
           AND judge.request_id = escalation.request_id
         WHERE escalation.turn_id = lifecycle.turn_id
           AND escalation.session_id = lifecycle.session_id
           AND escalation.terminal_attempt_id = lifecycle.terminal_attempt_id
           AND judge.state_kind = 'terminal'
           AND judge.terminal_disposition_kind = 'completed'
           AND judge.recommendation_kind = 'escalate_to_human'
    ) THEN
        RAISE EXCEPTION
            'failed tool-loop turn % lacks its exact terminal execution cause',
            checked_turn_id
            USING ERRCODE = '23514';
    END IF;
END;
$$;
