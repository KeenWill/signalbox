-- Preserve execution failures that cannot make progress under an unchanged
-- goal context, so goal disposition can park them instead of scheduling the
-- ordinary bounded automatic resumption loop.

CREATE TABLE goal_execution_failure_recovery (
    turn_id uuid PRIMARY KEY,
    session_id uuid NOT NULL,
    cause_kind text NOT NULL,
    recorded_at timestamptz NOT NULL DEFAULT transaction_timestamp(),

    UNIQUE (turn_id, session_id),
    CONSTRAINT goal_execution_failure_recovery_cause_kind_closed CHECK (
        cause_kind IN ('context_compaction_input_does_not_fit')
    ),
    CONSTRAINT goal_execution_failure_recovery_turn_fk
        FOREIGN KEY (turn_id, session_id)
        REFERENCES turn_lifecycle (turn_id, session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE FUNCTION require_goal_execution_failure_recovery_terminal()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM turn_lifecycle AS lifecycle
         WHERE lifecycle.turn_id = NEW.turn_id
           AND lifecycle.session_id = NEW.session_id
           AND lifecycle.state_kind = 'terminal'
           AND lifecycle.terminal_disposition_kind = 'failed'
           AND lifecycle.terminal_model_call_id IS NULL
    ) THEN
        RAISE EXCEPTION 'goal execution-failure recovery requires its exact call-free failed turn'
            USING
                ERRCODE = '23514',
                CONSTRAINT = 'goal_execution_failure_recovery_exact_terminal';
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER goal_execution_failure_recovery_requires_terminal
AFTER INSERT ON goal_execution_failure_recovery
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_goal_execution_failure_recovery_terminal();

CREATE TRIGGER goal_execution_failure_recovery_is_append_only
BEFORE UPDATE OR DELETE ON goal_execution_failure_recovery
FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE FUNCTION reject_goal_execution_failure_recovery_truncate()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION '% cannot be truncated', TG_TABLE_NAME
        USING ERRCODE = '23514';
END;
$$;

CREATE TRIGGER goal_execution_failure_recovery_reject_truncate
BEFORE TRUNCATE ON goal_execution_failure_recovery
FOR EACH STATEMENT
EXECUTE FUNCTION reject_goal_execution_failure_recovery_truncate();
