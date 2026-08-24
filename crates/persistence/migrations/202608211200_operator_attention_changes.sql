-- Durable, timestamped invalidation journal for the bounded operator fleet view.
-- It records observation facts only; no runtime decision reads this table.

CREATE TABLE operator_attention_change (
    change_sequence bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    session_id uuid NOT NULL REFERENCES session(session_id) ON DELETE RESTRICT,
    fact_kind text NOT NULL CHECK (
        fact_kind IN ('session', 'turn', 'goal', 'approval_judge', 'runner')
    ),
    recorded_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    CHECK (change_sequence > 0)
);

CREATE INDEX operator_attention_change_by_session_sequence
    ON operator_attention_change (session_id, change_sequence DESC);

CREATE FUNCTION serialize_operator_attention_change()
RETURNS void
LANGUAGE plpgsql
AS $$
BEGIN
    -- Preserve the repository-wide allocator-before-projection lock order.
    -- Re-locking the singleton from an outbox trigger is transaction-local.
    PERFORM 1 FROM outbox_sequence_state WHERE singleton FOR UPDATE;
    -- Identity values are allocated before commit. Serialize allocation so the
    -- greatest visible sequence is also a commit-safe follow frontier.
    PERFORM pg_advisory_xact_lock(1091, 1);
END;
$$;

-- Keep bounded approval-judge totals independent of session lifetime. Backfill
-- once, then maintain four scalars for each affected session.
ALTER TABLE session_timeline_fact
    ADD COLUMN approval_judge_actionable_count numeric(20, 0) NOT NULL DEFAULT 0,
    ADD COLUMN approval_judge_completed_count numeric(20, 0) NOT NULL DEFAULT 0,
    ADD COLUMN approval_judge_escalated_count numeric(20, 0) NOT NULL DEFAULT 0,
    ADD COLUMN approval_judge_failed_count numeric(20, 0) NOT NULL DEFAULT 0,
    ADD CONSTRAINT session_timeline_fact_approval_judge_actionable_u64 CHECK (
        approval_judge_actionable_count BETWEEN 0 AND 18446744073709551615
    ),
    ADD CONSTRAINT session_timeline_fact_approval_judge_completed_u64 CHECK (
        approval_judge_completed_count BETWEEN 0 AND 18446744073709551615
    ),
    ADD CONSTRAINT session_timeline_fact_approval_judge_escalated_u64 CHECK (
        approval_judge_escalated_count BETWEEN 0 AND 18446744073709551615
    ),
    ADD CONSTRAINT session_timeline_fact_approval_judge_failed_u64 CHECK (
        approval_judge_failed_count BETWEEN 0 AND 18446744073709551615
    );

WITH judge AS (
    SELECT call.session_id,
           count(*) FILTER (WHERE call.state_kind <> 'terminal') AS actionable,
           count(*) FILTER (
               WHERE call.terminal_disposition_kind = 'completed'
                 AND call.recommendation_kind <> 'escalate_to_human'
           ) AS completed,
           count(*) FILTER (
               WHERE call.terminal_disposition_kind = 'completed'
                 AND call.recommendation_kind = 'escalate_to_human'
           ) AS escalated,
           count(*) FILTER (
               WHERE call.state_kind = 'terminal'
                 AND call.terminal_disposition_kind <> 'completed'
           ) AS failed
      FROM tool_approval_judge_model_call AS call
     GROUP BY call.session_id
)
UPDATE session_timeline_fact AS fact
   SET approval_judge_actionable_count = judge.actionable,
       approval_judge_completed_count = judge.completed,
       approval_judge_escalated_count = judge.escalated,
       approval_judge_failed_count = judge.failed
  FROM judge
 WHERE judge.session_id = fact.session_id;

CREATE FUNCTION record_operator_attention_outbox_change()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    PERFORM serialize_operator_attention_change();
    INSERT INTO operator_attention_change (session_id, fact_kind)
    VALUES (
        NEW.session_id,
        CASE
            WHEN NEW.event_kind IN (
                'session_created',
                'session_model_settings_changed'
            ) THEN 'session'
            WHEN NEW.event_kind = 'goal_turn_retired' THEN 'goal'
            WHEN NEW.event_kind = 'runner_state_transition' THEN 'runner'
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

CREATE FUNCTION record_operator_attention_metadata_change()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    PERFORM serialize_operator_attention_change();
    INSERT INTO operator_attention_change (session_id, fact_kind)
    VALUES (COALESCE(NEW.session_id, OLD.session_id), 'session');
    RETURN NULL;
END;
$$;

CREATE TRIGGER session_metadata_records_operator_attention_change
AFTER INSERT OR UPDATE ON session_metadata
FOR EACH ROW EXECUTE FUNCTION record_operator_attention_metadata_change();

CREATE TRIGGER session_metadata_tag_records_operator_attention_change
AFTER INSERT OR DELETE ON session_metadata_tag
FOR EACH ROW EXECUTE FUNCTION record_operator_attention_metadata_change();

CREATE FUNCTION record_operator_attention_goal_change()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    PERFORM serialize_operator_attention_change();
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
    PERFORM serialize_operator_attention_change();
    IF TG_OP = 'UPDATE' THEN
        UPDATE session_timeline_fact
           SET approval_judge_actionable_count = approval_judge_actionable_count
                   - (OLD.state_kind <> 'terminal')::integer,
               approval_judge_completed_count = approval_judge_completed_count
                   - COALESCE(OLD.terminal_disposition_kind = 'completed'
                      AND OLD.recommendation_kind <> 'escalate_to_human', false)::integer,
               approval_judge_escalated_count = approval_judge_escalated_count
                   - COALESCE(OLD.terminal_disposition_kind = 'completed'
                      AND OLD.recommendation_kind = 'escalate_to_human', false)::integer,
               approval_judge_failed_count = approval_judge_failed_count
                   - COALESCE(OLD.state_kind = 'terminal'
                      AND OLD.terminal_disposition_kind <> 'completed', false)::integer
         WHERE session_id = OLD.session_id;
    END IF;
    UPDATE session_timeline_fact
       SET approval_judge_actionable_count = approval_judge_actionable_count
               + (NEW.state_kind <> 'terminal')::integer,
           approval_judge_completed_count = approval_judge_completed_count
               + COALESCE(NEW.terminal_disposition_kind = 'completed'
                  AND NEW.recommendation_kind <> 'escalate_to_human', false)::integer,
           approval_judge_escalated_count = approval_judge_escalated_count
               + COALESCE(NEW.terminal_disposition_kind = 'completed'
                  AND NEW.recommendation_kind = 'escalate_to_human', false)::integer,
           approval_judge_failed_count = approval_judge_failed_count
               + COALESCE(NEW.state_kind = 'terminal'
                  AND NEW.terminal_disposition_kind <> 'completed', false)::integer
     WHERE session_id = NEW.session_id;
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
    PERFORM serialize_operator_attention_change();
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
