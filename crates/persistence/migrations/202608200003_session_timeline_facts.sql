-- Incrementally maintained facts keep bounded timeline reads independent of
-- session lifetime. The one-time backfill is migration work; ordinary reads
-- and writes thereafter touch one fact row per affected session.
CREATE TABLE session_timeline_fact (
    session_id uuid PRIMARY KEY REFERENCES session(session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT,
    item_count numeric(20, 0) NOT NULL,
    first_sequence numeric(20, 0),
    latest_sequence numeric(20, 0),
    event_kind_bytes numeric(20, 0) NOT NULL,
    projected_text_bytes numeric(20, 0) NOT NULL,
    active_turn_count numeric(20, 0) NOT NULL,
    queued_turn_count numeric(20, 0) NOT NULL,
    CHECK (item_count >= 0 AND item_count <= 18446744073709551615),
    CHECK (event_kind_bytes >= 0 AND event_kind_bytes <= 18446744073709551615),
    CHECK (projected_text_bytes >= 0 AND projected_text_bytes <= 18446744073709551615),
    CHECK (active_turn_count >= 0 AND active_turn_count <= 18446744073709551615),
    CHECK (queued_turn_count >= 0 AND queued_turn_count <= 18446744073709551615),
    CHECK ((item_count = 0) = (first_sequence IS NULL AND latest_sequence IS NULL)),
    CHECK (first_sequence IS NULL OR (first_sequence >= 1 AND first_sequence <= latest_sequence)),
    CHECK (latest_sequence IS NULL OR latest_sequence <= 18446744073709551615)
);

INSERT INTO session_timeline_fact (
    session_id, item_count, first_sequence, latest_sequence,
    event_kind_bytes, projected_text_bytes,
    active_turn_count, queued_turn_count
)
SELECT s.session_id,
       count(events.event_sequence)::numeric,
       min(events.event_sequence),
       max(events.event_sequence),
       coalesce(sum(octet_length(events.event_kind)), 0)::numeric,
       (
           coalesce((SELECT sum(octet_length(convert_to(input.content_text, 'UTF8')))::numeric
                       FROM accepted_input AS input
                      WHERE input.session_id = s.session_id), 0)
           + coalesce((SELECT sum(octet_length(convert_to(entry.assistant_text_value, 'UTF8')))::numeric
                         FROM semantic_transcript_entry AS entry
                        WHERE entry.source_session_id = s.session_id
                          AND entry.assistant_text_value IS NOT NULL), 0)
           + coalesce((SELECT sum(octet_length(convert_to(entry.context_summary_value, 'UTF8')))::numeric
                         FROM semantic_transcript_entry AS entry
                        WHERE entry.source_session_id = s.session_id
                          AND entry.context_summary_value IS NOT NULL), 0)
       ),
       (SELECT count(*)::numeric FROM turn_lifecycle AS turn
         WHERE turn.session_id = s.session_id
           AND turn.state_kind = 'active'
           AND NOT turn.delegation_runtime_terminal),
       (SELECT count(*)::numeric FROM turn_lifecycle AS turn
         WHERE turn.session_id = s.session_id
           AND turn.state_kind = 'queued'
           AND NOT turn.delegation_runtime_terminal
           AND goal_turn_is_runtime_relevant(turn.session_id, turn.turn_id))
  FROM session AS s
  LEFT JOIN (
      SELECT event_sequence, event_kind, session_id FROM outbox_event
      UNION ALL
      SELECT event_sequence, event_kind, session_id FROM delegation_outbox_event
  ) AS events ON events.session_id = s.session_id
 GROUP BY s.session_id;

CREATE FUNCTION initialize_session_timeline_fact()
RETURNS trigger LANGUAGE plpgsql
SET search_path FROM CURRENT AS $$
BEGIN
    INSERT INTO session_timeline_fact (
        session_id, item_count, event_kind_bytes,
        projected_text_bytes, active_turn_count, queued_turn_count
    ) VALUES (NEW.session_id, 0, 0, 0, 0, 0);
    RETURN NULL;
END
$$;

CREATE TRIGGER session_initializes_timeline_fact
AFTER INSERT ON session
FOR EACH ROW EXECUTE FUNCTION initialize_session_timeline_fact();

CREATE FUNCTION append_session_timeline_event_fact()
RETURNS trigger LANGUAGE plpgsql
SET search_path FROM CURRENT AS $$
BEGIN
    UPDATE session_timeline_fact
       SET item_count = item_count + 1,
           first_sequence = coalesce(first_sequence, NEW.event_sequence),
           latest_sequence = NEW.event_sequence,
           event_kind_bytes = event_kind_bytes + octet_length(NEW.event_kind)
     WHERE session_id = NEW.session_id;
    RETURN NULL;
END
$$;

CREATE TRIGGER outbox_event_updates_timeline_fact
AFTER INSERT ON outbox_event
FOR EACH ROW EXECUTE FUNCTION append_session_timeline_event_fact();
CREATE TRIGGER delegation_outbox_event_updates_timeline_fact
AFTER INSERT ON delegation_outbox_event
FOR EACH ROW EXECUTE FUNCTION append_session_timeline_event_fact();

CREATE FUNCTION append_session_timeline_input_bytes()
RETURNS trigger LANGUAGE plpgsql
SET search_path FROM CURRENT AS $$
BEGIN
    -- Submission later acquires the allocator through lifecycle/outbox work.
    -- Preserve the global allocator-then-session-fact lock order here too.
    PERFORM 1 FROM outbox_sequence_state WHERE singleton FOR UPDATE;
    UPDATE session_timeline_fact
       SET projected_text_bytes = projected_text_bytes
           + octet_length(convert_to(NEW.content_text, 'UTF8'))
     WHERE session_id = NEW.session_id;
    RETURN NULL;
END
$$;

CREATE TRIGGER accepted_input_updates_timeline_fact
AFTER INSERT ON accepted_input
FOR EACH ROW EXECUTE FUNCTION append_session_timeline_input_bytes();

CREATE FUNCTION append_session_timeline_transcript_bytes()
RETURNS trigger LANGUAGE plpgsql
SET search_path FROM CURRENT AS $$
BEGIN
    IF NEW.assistant_text_value IS NULL AND NEW.context_summary_value IS NULL THEN
        RETURN NULL;
    END IF;
    -- Transcript persistence can share a transaction with later outbox work.
    -- Preserve the global allocator-then-session-fact lock order here too.
    PERFORM 1 FROM outbox_sequence_state WHERE singleton FOR UPDATE;
    UPDATE session_timeline_fact
       SET projected_text_bytes = projected_text_bytes
           + coalesce(octet_length(convert_to(NEW.assistant_text_value, 'UTF8')), 0)
           + coalesce(octet_length(convert_to(NEW.context_summary_value, 'UTF8')), 0)
     WHERE session_id = NEW.source_session_id;
    RETURN NULL;
END
$$;

CREATE TRIGGER semantic_transcript_entry_updates_timeline_fact
AFTER INSERT ON semantic_transcript_entry
FOR EACH ROW EXECUTE FUNCTION append_session_timeline_transcript_bytes();

CREATE FUNCTION update_session_timeline_work_fact()
RETURNS trigger LANGUAGE plpgsql
SET search_path FROM CURRENT AS $$
BEGIN
    -- Outbox appends acquire the allocator before this session fact. Taking the
    -- same locks in the same order prevents lifecycle updates from inverting it.
    PERFORM 1 FROM outbox_sequence_state WHERE singleton FOR UPDATE;
    IF TG_OP = 'UPDATE' THEN
        UPDATE session_timeline_fact
           SET active_turn_count = active_turn_count
                   - (OLD.state_kind = 'active' AND NOT OLD.delegation_runtime_terminal)::integer
                   + (NEW.state_kind = 'active' AND NOT NEW.delegation_runtime_terminal)::integer,
               queued_turn_count = queued_turn_count
                   - (OLD.state_kind = 'queued' AND NOT OLD.delegation_runtime_terminal)::integer
                   + (NEW.state_kind = 'queued' AND NOT NEW.delegation_runtime_terminal)::integer
         WHERE session_id = NEW.session_id;
    ELSE
        UPDATE session_timeline_fact
           SET active_turn_count = active_turn_count
                   + (NEW.state_kind = 'active' AND NOT NEW.delegation_runtime_terminal)::integer,
               queued_turn_count = queued_turn_count
                   + (NEW.state_kind = 'queued' AND NOT NEW.delegation_runtime_terminal)::integer
         WHERE session_id = NEW.session_id;
    END IF;
    RETURN NULL;
END
$$;

CREATE TRIGGER turn_lifecycle_updates_timeline_fact
AFTER INSERT OR UPDATE OF state_kind, delegation_runtime_terminal ON turn_lifecycle
FOR EACH ROW EXECUTE FUNCTION update_session_timeline_work_fact();

CREATE FUNCTION retire_session_timeline_goal_work_fact()
RETURNS trigger LANGUAGE plpgsql
SET search_path FROM CURRENT AS $$
DECLARE
    retired_queued_count numeric(20, 0);
BEGIN
    IF NEW.event_kind NOT IN ('user_stopped', 'superseded') THEN
        RETURN NULL;
    END IF;

    SELECT count(*)::numeric
      INTO retired_queued_count
      FROM goal_turn AS goal
      JOIN turn_lifecycle AS lifecycle
        ON lifecycle.session_id = goal.session_id
       AND lifecycle.turn_id = goal.turn_id
     WHERE goal.session_id = NEW.session_id
       AND goal.goal_generation = NEW.generation
       AND lifecycle.state_kind = 'queued'
       AND NOT lifecycle.delegation_runtime_terminal;

    IF retired_queued_count > 0 THEN
        -- Goal retirement later appends an outbox event. Preserve the same
        -- allocator-then-fact lock order used by every other fact update.
        PERFORM 1 FROM outbox_sequence_state WHERE singleton FOR UPDATE;
        UPDATE session_timeline_fact
           SET queued_turn_count = queued_turn_count - retired_queued_count
         WHERE session_id = NEW.session_id;
    END IF;
    RETURN NULL;
END
$$;

CREATE TRIGGER goal_event_retires_timeline_work_fact
AFTER INSERT ON goal_event
FOR EACH ROW EXECUTE FUNCTION retire_session_timeline_goal_work_fact();
