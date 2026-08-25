-- Durable, timestamped invalidation journal for the bounded operator fleet view.
-- It records observation facts only; no runtime decision reads this table.

CREATE SEQUENCE operator_attention_change_sequence AS bigint;

CREATE FUNCTION next_operator_attention_change_sequence()
RETURNS bigint
LANGUAGE plpgsql
AS $$
BEGIN
    -- Outbox-producing transactions already hold this row through commit.
    -- Direct attention facts take the same lock, so every publisher uses one
    -- lock order and attention cursors remain commit-monotonic.
    PERFORM 1
      FROM outbox_sequence_state
     WHERE singleton
     FOR UPDATE;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'attention sequence requires outbox sequence state'
            USING ERRCODE = '23503';
    END IF;
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
    recorded_at timestamptz NOT NULL DEFAULT clock_timestamp(),
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
            WHEN 'goal_turn_retired' THEN 'goal'
            WHEN 'runner_state_transition' THEN 'runner'
            WHEN 'delegation_update' THEN 'turn'
            WHEN 'delegation_wake' THEN 'turn'
            WHEN 'turn_model_settings_resolved' THEN 'turn'
            WHEN 'input_accepted' THEN 'turn'
            WHEN 'turn_activated' THEN 'turn'
            WHEN 'turn_failed' THEN 'turn'
            WHEN 'model_call_transition' THEN 'turn'
            WHEN 'tool_batch_transition' THEN 'turn'
            WHEN 'tool_approval_decided' THEN 'turn'
            WHEN 'context_compacted' THEN 'turn'
            WHEN 'turn_completed' THEN 'turn'
            WHEN 'turn_refused' THEN 'turn'
            WHEN 'turn_cancelled' THEN 'turn'
            WHEN 'turn_reconciliation_required' THEN 'turn'
            ELSE NULL
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

-- Rejected goal commands append no goal event, but a rejected automatic
-- resumption transfers the blocked goal back to operator ownership. Publish
-- that durable outcome so existing followers refresh the affected summary.
CREATE TRIGGER rejected_goal_command_records_operator_attention_change
AFTER INSERT ON goal_command
FOR EACH ROW
WHEN (NEW.result_kind = 'rejected')
EXECUTE FUNCTION record_operator_attention_goal_change();

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

-- Keep the bounded fleet read independent of unbounded approval history.
CREATE TABLE operator_attention_judge_facts (
    session_id uuid PRIMARY KEY REFERENCES session(session_id) ON DELETE RESTRICT,
    actionable bigint NOT NULL CHECK (actionable >= 0),
    completed bigint NOT NULL CHECK (completed >= 0),
    escalated bigint NOT NULL CHECK (escalated >= 0),
    failed bigint NOT NULL CHECK (failed >= 0)
);

INSERT INTO operator_attention_judge_facts
    (session_id, actionable, completed, escalated, failed)
SELECT call.session_id,
       count(*) FILTER (WHERE call.state_kind <> 'terminal'),
       count(*) FILTER (WHERE call.terminal_disposition_kind = 'completed'
                          AND call.recommendation_kind <> 'escalate_to_human'),
       count(*) FILTER (WHERE call.terminal_disposition_kind = 'completed'
                          AND call.recommendation_kind = 'escalate_to_human'),
       count(*) FILTER (WHERE call.state_kind = 'terminal'
                          AND call.terminal_disposition_kind <> 'completed')
  FROM tool_approval_judge_model_call AS call
 GROUP BY call.session_id;

CREATE FUNCTION update_operator_attention_judge_facts()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    old_actionable bigint := 0;
    old_completed bigint := 0;
    old_escalated bigint := 0;
    old_failed bigint := 0;
    new_actionable bigint;
    new_completed bigint;
    new_escalated bigint;
    new_failed bigint;
BEGIN
    IF TG_OP = 'UPDATE' THEN
        old_actionable := (OLD.state_kind <> 'terminal')::integer;
        old_completed := COALESCE((OLD.terminal_disposition_kind = 'completed'
            AND OLD.recommendation_kind <> 'escalate_to_human')::integer, 0);
        old_escalated := COALESCE((OLD.terminal_disposition_kind = 'completed'
            AND OLD.recommendation_kind = 'escalate_to_human')::integer, 0);
        old_failed := COALESCE((OLD.state_kind = 'terminal'
            AND OLD.terminal_disposition_kind <> 'completed')::integer, 0);
    END IF;
    new_actionable := (NEW.state_kind <> 'terminal')::integer;
    new_completed := COALESCE((NEW.terminal_disposition_kind = 'completed'
        AND NEW.recommendation_kind <> 'escalate_to_human')::integer, 0);
    new_escalated := COALESCE((NEW.terminal_disposition_kind = 'completed'
        AND NEW.recommendation_kind = 'escalate_to_human')::integer, 0);
    new_failed := COALESCE((NEW.state_kind = 'terminal'
        AND NEW.terminal_disposition_kind <> 'completed')::integer, 0);

    -- Seed the counter row before applying the deltas, then add them with an
    -- UPDATE. Carrying a delta through the INSERT of an upsert cannot work
    -- here: PostgreSQL validates CHECK constraints against the proposed insert
    -- tuple before the ON CONFLICT arbiter runs, so a transition that retires
    -- an actionable call (delta -1) is rejected by `actionable >= 0` even when
    -- the conflicting row makes the resulting sum non-negative. The seed tuple
    -- is all zeros and so always satisfies the constraints, and the UPDATE
    -- still checks the summed row -- which is the invariant worth holding.
    INSERT INTO operator_attention_judge_facts
        (session_id, actionable, completed, escalated, failed)
    VALUES (NEW.session_id, 0, 0, 0, 0)
    ON CONFLICT (session_id) DO NOTHING;

    UPDATE operator_attention_judge_facts
       SET actionable = actionable + (new_actionable - old_actionable),
           completed = completed + (new_completed - old_completed),
           escalated = escalated + (new_escalated - old_escalated),
           failed = failed + (new_failed - old_failed)
     WHERE session_id = NEW.session_id;
    RETURN NULL;
END;
$$;

CREATE TRIGGER tool_approval_judge_updates_operator_attention_facts
AFTER INSERT OR UPDATE ON tool_approval_judge_model_call
FOR EACH ROW EXECUTE FUNCTION update_operator_attention_judge_facts();

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

-- Existing user-created sessions use their authoritative command-claim time.
-- Delegated children have no creation command; their mandatory version-one
-- placement is written in the spawning transaction and supplies its durable
-- creation timestamp. No historical time is inferred from UUID identity bits.
WITH creation_activity AS (
    SELECT creation.created_session_id AS session_id,
           command.claimed_at AS recorded_at
      FROM create_session_command AS creation
      JOIN durable_command AS command USING (command_id)
    UNION ALL
    SELECT creation.created_session_id, command.claimed_at
      FROM create_session_from_imported_frontier_command AS creation
      JOIN durable_command AS command USING (command_id)
    UNION ALL
    SELECT delegated.session_id, placement.recorded_at
      FROM session AS delegated
      JOIN session_placement_event AS placement
        ON placement.session_id = delegated.session_id
       AND placement.version = 1
       AND placement.prior_version IS NULL
       AND placement.event_kind = 'created'
     WHERE delegated.creation_cause = 'delegated'
)
INSERT INTO operator_attention_change (session_id, fact_kind, recorded_at)
SELECT creation.session_id, 'session', creation.recorded_at
  FROM creation_activity AS creation;

CREATE TRIGGER operator_attention_change_is_append_only
BEFORE UPDATE OR DELETE ON operator_attention_change
FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER operator_attention_change_rejects_truncate
BEFORE TRUNCATE ON operator_attention_change
FOR EACH STATEMENT EXECUTE FUNCTION reject_immutable_record_change();
