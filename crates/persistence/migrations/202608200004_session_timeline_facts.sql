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

-- `goal_turn_is_queue_order_relevant` answers this for a stored lifecycle row,
-- and reads that row's `state_kind` to do it. The triggers below have to ask
-- about the state a row is *leaving*, which that function cannot report once
-- the row has already changed, so they need the goal half of the predicate on
-- its own: is this turn's goal generation the one the session still pursues?
-- Every branch mirrors that function exactly, including both of its admissions
-- of turns no goal event speaks for -- a turn with no `goal_turn` row, and a
-- goal turn in a session with no goal event yet.
CREATE FUNCTION goal_turn_generation_is_pursued(
    checked_session uuid,
    checked_turn uuid
)
RETURNS boolean
LANGUAGE sql
STABLE
SET search_path FROM CURRENT AS $$
    SELECT coalesce((
        SELECT (
            SELECT (
                event.event_kind IN ('commissioned', 'resumed')
                AND event.generation = goal.goal_generation
            ) OR (
                event.event_kind = 'superseded'
                AND event.generation < 18446744073709551615
                AND event.generation + 1 = goal.goal_generation
            )
              FROM goal_event AS event
             WHERE event.session_id = checked_session
             ORDER BY event.event_ordinal DESC
             LIMIT 1
        )
          FROM goal_turn AS goal
         WHERE goal.session_id = checked_session
           AND goal.turn_id = checked_turn
    ), true);
$$;

-- The generation a goal event leaves pursued, or NULL when it retires the
-- session's goal outright. `commissioned` and `resumed` pursue the generation
-- they name, `superseded` pursues the next one, and `blocked`, `achieved` and
-- `user_stopped` pursue none. Naming the pursued generation rather than the
-- event kind is what lets the reconciliation below work in bounded deltas:
-- relevance is generation equality against this one value, so a transition can
-- only move turns between the generation it retires and the one it pursues.
CREATE FUNCTION goal_event_pursued_generation(
    checked_kind text,
    checked_generation numeric
)
RETURNS numeric
LANGUAGE sql
IMMUTABLE
SET search_path FROM CURRENT AS $$
    SELECT CASE
             WHEN checked_kind IN ('commissioned', 'resumed')
                 THEN checked_generation
             WHEN checked_kind = 'superseded'
                  AND checked_generation < 18446744073709551615
                 THEN checked_generation + 1
           END;
$$;

CREATE FUNCTION update_session_timeline_work_fact()
RETURNS trigger LANGUAGE plpgsql
SET search_path FROM CURRENT AS $$
DECLARE
    pursued boolean;
BEGIN
    -- Outbox appends acquire the allocator before this session fact. Taking the
    -- same locks in the same order prevents lifecycle updates from inverting it.
    PERFORM 1 FROM outbox_sequence_state WHERE singleton FOR UPDATE;
    -- A queued turn is credited only while its goal generation is still
    -- pursued, so the subtraction has to carry the same guard the credit did.
    -- Without it a turn already retired by a goal event -- whose credit the
    -- reconciliation below has removed -- is subtracted a second time when it
    -- later leaves the queue or releases its delegated runtime slot, driving
    -- the count negative and aborting the writing transaction on the fact
    -- table's nonnegative check.
    --
    -- This trigger fires only on `state_kind` and `delegation_runtime_terminal`
    -- and neither changes which generation the session pursues, so a single
    -- evaluation is correct for the old and the new state alike. It has to
    -- happen under the allocator lock: that lock is what serialises this
    -- against a goal event retiring the same generation concurrently, and a
    -- read taken before it could credit a generation the committed goal event
    -- has already retired.
    pursued := goal_turn_generation_is_pursued(NEW.session_id, NEW.turn_id);
    IF TG_OP = 'UPDATE' THEN
        UPDATE session_timeline_fact
           SET active_turn_count = active_turn_count
                   - (OLD.state_kind = 'active' AND NOT OLD.delegation_runtime_terminal)::integer
                   + (NEW.state_kind = 'active' AND NOT NEW.delegation_runtime_terminal)::integer,
               queued_turn_count = queued_turn_count
                   - (OLD.state_kind = 'queued'
                      AND NOT OLD.delegation_runtime_terminal
                      AND pursued)::integer
                   + (NEW.state_kind = 'queued'
                      AND NOT NEW.delegation_runtime_terminal
                      AND pursued)::integer
         WHERE session_id = NEW.session_id;
    ELSE
        UPDATE session_timeline_fact
           SET active_turn_count = active_turn_count
                   + (NEW.state_kind = 'active' AND NOT NEW.delegation_runtime_terminal)::integer,
               queued_turn_count = queued_turn_count
                   + (NEW.state_kind = 'queued'
                      AND NOT NEW.delegation_runtime_terminal
                      AND pursued)::integer
         WHERE session_id = NEW.session_id;
    END IF;
    RETURN NULL;
END
$$;

CREATE TRIGGER turn_lifecycle_updates_timeline_fact
AFTER INSERT OR UPDATE OF state_kind, delegation_runtime_terminal ON turn_lifecycle
FOR EACH ROW EXECUTE FUNCTION update_session_timeline_work_fact();

-- Goal events change which queued turns count as pursued work, so this applies
-- the delta that change is worth. Rescanning the session instead would make an
-- ordinary goal transition cost one `goal_turn_is_runtime_relevant` call per
-- queued turn the session ever accumulated, under the global allocator lock --
-- lifetime-linear work that stalls outbox allocation for unrelated sessions and
-- defeats the bounded-write purpose of maintaining the fact at all.
--
-- The delta is keyed on the pursued generation rather than on the event kind,
-- which is what makes it agree with the backfill above for every kind. Under
-- `goal_turn_is_queue_order_relevant` a queued goal turn is relevant exactly
-- when its generation equals the one the session's single latest goal event
-- leaves pursued, so `blocked` and `achieved` retire a generation just as
-- `user_stopped` and `superseded` do, and a later `resumed` restores it. Only
-- two generations can therefore change hands -- the one the previous event left
-- pursued and the one this event pursues -- and every other queued turn keeps
-- the credit it already had.
--
-- Keying it this way also disposes of the divergence an event-kind delta had.
-- `blocked` may be followed by `user_stopped` at that same generation: both
-- leave nothing pursued, the transition moves no turn, and the delta is zero
-- rather than a second subtraction driving the count to -1 and tripping
-- `CHECK (queued_turn_count >= 0)`.
CREATE FUNCTION reconcile_session_timeline_goal_work_fact()
RETURNS trigger LANGUAGE plpgsql
SET search_path FROM CURRENT AS $$
DECLARE
    prior_kind text;
    prior_generation numeric(20, 0);
    retired numeric(20, 0);
    pursued numeric(20, 0);
    gained bigint;
    lost bigint;
BEGIN
    IF NEW.event_ordinal = 1 THEN
        -- The session's first goal event. No committed goal turn can precede it
        -- because a goal turn requires a goal event, and any goal turn inserted
        -- alongside it must belong to the generation it commissions, since
        -- `goal_turn_current_pursuit` is checked against the latest event at
        -- commit. Such a turn was admitted before this event by the same "no
        -- goal event speaks for it" fallback that admits it after, so nothing
        -- changes hands and the allocator is not worth touching.
        RETURN NULL;
    END IF;
    -- `goal_event` is append-only with contiguous ordinals, serialised per
    -- session by the row lock `require_goal_event_continuity` already holds, so
    -- the event this one displaces as latest is a primary-key lookup.
    SELECT event_kind, generation INTO prior_kind, prior_generation
      FROM goal_event
     WHERE session_id = NEW.session_id
       AND event_ordinal = NEW.event_ordinal - 1;
    retired := goal_event_pursued_generation(prior_kind, prior_generation);
    pursued := goal_event_pursued_generation(NEW.event_kind, NEW.generation);
    IF retired IS NOT DISTINCT FROM pursued THEN
        -- The session pursues what it already pursued, so no queued turn
        -- changes hands. This decision reads only `goal_event`, which the
        -- session row lock above already serialises, so returning here without
        -- the allocator keeps a restated retirement off the global lock.
        RETURN NULL;
    END IF;

    -- Everything below runs under the allocator lock, in the same
    -- allocator-then-fact order every other fact update takes. The counts have
    -- to be read under it rather than before it: that lock is what serialises
    -- this against a lifecycle transition moving the same turns concurrently,
    -- and a delta computed from an earlier read could subtract a turn the other
    -- transaction has already accounted for.
    PERFORM 1 FROM outbox_sequence_state WHERE singleton FOR UPDATE;
    -- Each count is keyed by generation, so it reads the goal-turn index on
    -- `(session_id, goal_generation, ...)` and touches only the turns this
    -- transition actually moves. A NULL generation names no turn and matches
    -- none, which is how the retiring kinds count zero on the pursued side.
    SELECT count(*) INTO gained
      FROM goal_turn AS goal
      JOIN turn_lifecycle AS turn
        ON turn.session_id = goal.session_id
       AND turn.turn_id = goal.turn_id
     WHERE goal.session_id = NEW.session_id
       AND goal.goal_generation = pursued
       AND turn.state_kind = 'queued'
       AND NOT turn.delegation_runtime_terminal;
    SELECT count(*) INTO lost
      FROM goal_turn AS goal
      JOIN turn_lifecycle AS turn
        ON turn.session_id = goal.session_id
       AND turn.turn_id = goal.turn_id
     WHERE goal.session_id = NEW.session_id
       AND goal.goal_generation = retired
       AND turn.state_kind = 'queued'
       AND NOT turn.delegation_runtime_terminal;
    IF gained = lost THEN
        RETURN NULL;
    END IF;
    UPDATE session_timeline_fact
       SET queued_turn_count = queued_turn_count + gained - lost
     WHERE session_id = NEW.session_id;
    RETURN NULL;
END
$$;

CREATE TRIGGER goal_event_reconciles_timeline_work_fact
AFTER INSERT ON goal_event
FOR EACH ROW EXECUTE FUNCTION reconcile_session_timeline_goal_work_fact();
