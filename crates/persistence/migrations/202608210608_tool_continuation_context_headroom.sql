-- A completed tool-producing call can consume enough provider-reported
-- context that another same-turn call cannot retain its configured output
-- reservation. Preserve that boundary as a typed append-only cause rather
-- than fabricating a provider failure.

CREATE TABLE tool_continuation_context_headroom (
    terminal_attempt_id uuid PRIMARY KEY,
    producing_model_call_id uuid NOT NULL UNIQUE,
    session_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    usage_input_includes_cache_tokens boolean NOT NULL,
    usage_input_tokens numeric(20, 0) NOT NULL,
    usage_output_tokens numeric(20, 0),
    usage_cache_creation_input_tokens numeric(20, 0),
    usage_cache_read_input_tokens numeric(20, 0),
    max_output_tokens numeric(20, 0) NOT NULL,
    context_window_tokens numeric(20, 0) NOT NULL,
    recorded_at timestamptz NOT NULL DEFAULT transaction_timestamp(),

    CONSTRAINT tool_continuation_context_headroom_usage_nonnegative CHECK (
        usage_input_tokens >= 0
        AND (usage_output_tokens IS NULL OR usage_output_tokens >= 0)
        AND (
            usage_cache_creation_input_tokens IS NULL
            OR usage_cache_creation_input_tokens >= 0
        )
        AND (
            usage_cache_read_input_tokens IS NULL
            OR usage_cache_read_input_tokens >= 0
        )
    ),
    CONSTRAINT tool_continuation_context_headroom_limits_positive CHECK (
        max_output_tokens > 0
        AND context_window_tokens > 0
        AND max_output_tokens <= context_window_tokens
    ),
    CONSTRAINT tool_continuation_context_headroom_proves_exhaustion CHECK (
        (
            usage_input_tokens
            + CASE
                WHEN usage_input_includes_cache_tokens THEN 0
                ELSE COALESCE(usage_cache_creation_input_tokens, 0)
                    + COALESCE(usage_cache_read_input_tokens, 0)
              END
            + COALESCE(usage_output_tokens, 0)
            + max_output_tokens
        ) > context_window_tokens
    ),
    CONSTRAINT tool_continuation_context_headroom_attempt_fk
        FOREIGN KEY (terminal_attempt_id, turn_id, session_id)
        REFERENCES turn_attempt (turn_attempt_id, turn_id, session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT tool_continuation_context_headroom_round_fk
        FOREIGN KEY (producing_model_call_id, turn_id, session_id)
        REFERENCES tool_round (producing_model_call_id, turn_id, session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE FUNCTION require_tool_continuation_context_headroom_terminal()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM turn_lifecycle AS lifecycle
          JOIN turn_attempt AS attempt
            ON attempt.turn_attempt_id = NEW.terminal_attempt_id
           AND attempt.turn_id = NEW.turn_id
           AND attempt.session_id = NEW.session_id
         WHERE lifecycle.turn_id = NEW.turn_id
           AND lifecycle.session_id = NEW.session_id
           AND lifecycle.state_kind = 'terminal'
           AND lifecycle.terminal_disposition_kind = 'failed'
           AND lifecycle.terminal_attempt_id = NEW.terminal_attempt_id
           AND lifecycle.terminal_model_call_id IS NULL
           AND attempt.state_kind = 'ended'
           AND attempt.end_variant = 'without_stop'
           AND attempt.end_disposition = 'known_failure'
    ) THEN
        RAISE EXCEPTION 'context-headroom marker requires its exact failed turn'
            USING
                ERRCODE = '23514',
                CONSTRAINT = 'tool_continuation_context_headroom_exact_terminal';
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER tool_continuation_context_headroom_requires_terminal
AFTER INSERT ON tool_continuation_context_headroom
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_tool_continuation_context_headroom_terminal();

CREATE TRIGGER tool_continuation_context_headroom_is_append_only
BEFORE UPDATE OR DELETE ON tool_continuation_context_headroom
FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE FUNCTION reject_tool_continuation_context_headroom_truncate()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION '% cannot be truncated', TG_TABLE_NAME
        USING ERRCODE = '23514';
END;
$$;

CREATE TRIGGER tool_continuation_context_headroom_reject_truncate
BEFORE TRUNCATE ON tool_continuation_context_headroom
FOR EACH STATEMENT
EXECUTE FUNCTION reject_tool_continuation_context_headroom_truncate();

ALTER FUNCTION assert_failed_terminal_execution_final_state(uuid)
    RENAME TO assert_failed_terminal_execution_before_context_headroom;

CREATE FUNCTION assert_failed_terminal_execution_final_state(checked_turn_id uuid)
RETURNS void
LANGUAGE plpgsql
AS $$
BEGIN
    IF EXISTS (
        SELECT 1
          FROM tool_continuation_context_headroom AS headroom
          JOIN turn_lifecycle AS lifecycle
            ON lifecycle.turn_id = headroom.turn_id
           AND lifecycle.session_id = headroom.session_id
          JOIN turn_attempt AS attempt
            ON attempt.turn_attempt_id = headroom.terminal_attempt_id
           AND attempt.turn_id = headroom.turn_id
           AND attempt.session_id = headroom.session_id
         WHERE headroom.turn_id = checked_turn_id
           AND lifecycle.state_kind = 'terminal'
           AND lifecycle.terminal_disposition_kind = 'failed'
           AND lifecycle.terminal_attempt_id = headroom.terminal_attempt_id
           AND lifecycle.terminal_model_call_id IS NULL
           AND attempt.state_kind = 'ended'
           AND attempt.end_variant = 'without_stop'
           AND attempt.end_disposition = 'known_failure'
    ) THEN
        RETURN;
    END IF;

    PERFORM assert_failed_terminal_execution_before_context_headroom(
        checked_turn_id
    );
END;
$$;
