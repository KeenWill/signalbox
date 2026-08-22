-- Durable, timestamped invalidation journal for the bounded operator fleet view.
-- It records observation facts only; no runtime decision reads this table.

CREATE SEQUENCE operator_attention_change_sequence AS bigint;

CREATE FUNCTION next_operator_attention_change_sequence()
RETURNS bigint
LANGUAGE plpgsql
AS $$
BEGIN
    PERFORM pg_advisory_xact_lock(
        hashtextextended('signalbox.operator_attention_change', 0)
    );
    RETURN nextval('operator_attention_change_sequence');
END;
$$;

CREATE TABLE operator_attention_change (
    change_sequence bigint PRIMARY KEY
        DEFAULT next_operator_attention_change_sequence(),
    session_id uuid NOT NULL REFERENCES session(session_id) ON DELETE RESTRICT,
    fact_kind text NOT NULL CHECK (
        fact_kind IN ('session', 'turn', 'goal', 'approval_judge', 'runner')
    ),
    recorded_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    CHECK (change_sequence > 0)
);

CREATE INDEX operator_attention_change_by_session_sequence
    ON operator_attention_change (session_id, change_sequence DESC);

CREATE FUNCTION record_operator_attention_outbox_change()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    INSERT INTO operator_attention_change (session_id, fact_kind)
    VALUES (
        NEW.session_id,
        CASE NEW.event_kind
            WHEN 'session_created' THEN 'session'
            WHEN 'session_model_settings_changed' THEN 'session'
            WHEN 'runner_state_transition' THEN 'runner'
            ELSE 'turn'
        END
    );
    RETURN NULL;
END;
$$;

CREATE TRIGGER outbox_event_records_operator_attention_change
AFTER INSERT ON outbox_event
FOR EACH ROW EXECUTE FUNCTION record_operator_attention_outbox_change();

CREATE TRIGGER delegation_outbox_event_records_operator_attention_change
AFTER INSERT ON delegation_outbox_event
FOR EACH ROW EXECUTE FUNCTION record_operator_attention_outbox_change();

CREATE FUNCTION record_operator_attention_goal_change()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    INSERT INTO operator_attention_change (session_id, fact_kind)
    VALUES (NEW.session_id, 'goal');
    RETURN NULL;
END;
$$;

CREATE TRIGGER goal_event_records_operator_attention_change
AFTER INSERT ON goal_event
FOR EACH ROW EXECUTE FUNCTION record_operator_attention_goal_change();

CREATE FUNCTION record_operator_attention_judge_change()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    INSERT INTO operator_attention_change (session_id, fact_kind)
    VALUES (NEW.session_id, 'approval_judge');
    RETURN NULL;
END;
$$;

CREATE TRIGGER tool_approval_judge_records_operator_attention_change
AFTER INSERT OR UPDATE ON tool_approval_judge_model_call
FOR EACH ROW EXECUTE FUNCTION record_operator_attention_judge_change();

CREATE FUNCTION record_operator_attention_runner_change()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    INSERT INTO operator_attention_change (session_id, fact_kind)
    VALUES (NEW.session_id, 'runner');
    RETURN NULL;
END;
$$;

CREATE TRIGGER runner_placement_records_operator_attention_change
AFTER INSERT ON runner_session_placement_record
FOR EACH ROW EXECUTE FUNCTION record_operator_attention_runner_change();

-- Existing sessions receive only their authoritative command-claim time. No
-- historical activity time is inferred from UUID identity bits.
WITH creation_commands AS (
    SELECT created_session_id AS session_id, command_id
      FROM create_session_command
    UNION ALL
    SELECT created_session_id, command_id
      FROM create_session_from_imported_frontier_command
)
INSERT INTO operator_attention_change (session_id, fact_kind, recorded_at)
SELECT creation.session_id, 'session', command.claimed_at
  FROM creation_commands AS creation
  JOIN durable_command AS command USING (command_id);

CREATE TRIGGER operator_attention_change_is_append_only
BEFORE UPDATE OR DELETE ON operator_attention_change
FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER operator_attention_change_rejects_truncate
BEFORE TRUNCATE ON operator_attention_change
FOR EACH STATEMENT EXECUTE FUNCTION reject_immutable_record_change();
