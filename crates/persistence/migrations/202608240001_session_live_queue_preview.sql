-- Live snapshots reach their bounded queue preview through an incrementally
-- maintained current-work relation instead of evaluating retained goal history.
CREATE TABLE session_live_queued_turn (
    session_id uuid NOT NULL,
    turn_id uuid NOT NULL UNIQUE,
    acceptance_position numeric(20, 0) NOT NULL,
    PRIMARY KEY (session_id, acceptance_position),
    FOREIGN KEY (turn_id, session_id, acceptance_position)
        REFERENCES turn_lifecycle (turn_id, session_id, acceptance_position)
        ON UPDATE CASCADE ON DELETE CASCADE
);

INSERT INTO session_live_queued_turn (session_id, turn_id, acceptance_position)
SELECT session_id, turn_id, acceptance_position
  FROM turn_lifecycle
 WHERE state_kind = 'queued'
   AND NOT delegation_runtime_terminal
   AND goal_turn_is_runtime_relevant(session_id, turn_id);

CREATE FUNCTION refresh_session_live_queued_turn(
    checked_session uuid,
    checked_turn uuid
)
RETURNS void
LANGUAGE plpgsql
SET search_path FROM CURRENT AS $$
DECLARE
    lifecycle turn_lifecycle%ROWTYPE;
BEGIN
    -- Every timeline-fact trigger takes the outbox allocator lock before its
    -- fact row, and trigger-name ordering runs this function's lifecycle
    -- trigger before the fact trigger while the goal-event fact trigger runs
    -- before the goal-event queue trigger. Taking the allocator first here
    -- keeps one global allocator-then-rows order, so a queued-turn transition
    -- concurrent with a goal event cannot deadlock.
    PERFORM 1 FROM outbox_sequence_state WHERE singleton FOR UPDATE;
    DELETE FROM session_live_queued_turn
     WHERE session_id = checked_session AND turn_id = checked_turn;

    SELECT * INTO lifecycle
      FROM turn_lifecycle
     WHERE session_id = checked_session AND turn_id = checked_turn;

    IF lifecycle.turn_id IS NOT NULL
       AND lifecycle.state_kind = 'queued'
       AND NOT lifecycle.delegation_runtime_terminal
       AND goal_turn_is_runtime_relevant(checked_session, checked_turn) THEN
        INSERT INTO session_live_queued_turn (
            session_id, turn_id, acceptance_position
        ) VALUES (
            lifecycle.session_id, lifecycle.turn_id, lifecycle.acceptance_position
        );
    END IF;
END
$$;

CREATE FUNCTION refresh_session_live_queued_turn_from_lifecycle()
RETURNS trigger
LANGUAGE plpgsql
SET search_path FROM CURRENT AS $$
BEGIN
    PERFORM refresh_session_live_queued_turn(NEW.session_id, NEW.turn_id);
    RETURN NULL;
END
$$;

CREATE TRIGGER turn_lifecycle_refreshes_session_live_queue
AFTER INSERT OR UPDATE OF state_kind, delegation_runtime_terminal ON turn_lifecycle
FOR EACH ROW EXECUTE FUNCTION refresh_session_live_queued_turn_from_lifecycle();

CREATE FUNCTION refresh_session_live_queued_turn_from_goal()
RETURNS trigger
LANGUAGE plpgsql
SET search_path FROM CURRENT AS $$
BEGIN
    PERFORM refresh_session_live_queued_turn(NEW.session_id, NEW.turn_id);
    RETURN NULL;
END
$$;

CREATE TRIGGER goal_turn_refreshes_session_live_queue
AFTER INSERT ON goal_turn
FOR EACH ROW EXECUTE FUNCTION refresh_session_live_queued_turn_from_goal();

-- Goal events change which queued turns count as pursued work. The delta is
-- keyed on the pursued generation, exactly as
-- `reconcile_session_timeline_goal_work_fact` keys the count: under
-- `goal_turn_is_runtime_relevant` a queued goal turn is relevant precisely
-- when its generation is the one the session's single latest goal event
-- leaves pursued, so `blocked` and `achieved` retire a generation just as
-- `user_stopped` and `superseded` do, a later `resumed` restores it, and only
-- the retired and pursued generations can change hands. Keying on generation
-- rather than event kind keeps the relation equal to that count for every
-- kind, and keeps the work bounded by the rows that actually move instead of
-- rescanning the session's retained history on every goal event.
CREATE FUNCTION refresh_session_live_goal_queue()
RETURNS trigger
LANGUAGE plpgsql
SET search_path FROM CURRENT AS $$
DECLARE
    prior_kind text;
    prior_generation numeric(20, 0);
    retired numeric(20, 0);
    pursued numeric(20, 0);
BEGIN
    IF NEW.event_ordinal = 1 THEN
        -- The session's first goal event commissions its own turn, whose
        -- lifecycle insert trigger adds the row; nothing changes hands.
        RETURN NULL;
    END IF;
    SELECT event_kind, generation INTO prior_kind, prior_generation
      FROM goal_event
     WHERE session_id = NEW.session_id
       AND event_ordinal = NEW.event_ordinal - 1;
    retired := goal_event_pursued_generation(prior_kind, prior_generation);
    pursued := goal_event_pursued_generation(NEW.event_kind, NEW.generation);
    IF retired IS NOT DISTINCT FROM pursued THEN
        RETURN NULL;
    END IF;
    -- Same allocator-then-rows lock order as refresh_session_live_queued_turn;
    -- the goal-event fact trigger that runs before this one already holds it.
    PERFORM 1 FROM outbox_sequence_state WHERE singleton FOR UPDATE;
    -- A NULL retired or pursued generation names no generation and moves no
    -- row, which is how the retiring kinds delete without inserting.
    DELETE FROM session_live_queued_turn AS queued
     USING goal_turn AS goal
     WHERE queued.session_id = NEW.session_id
       AND goal.session_id = queued.session_id
       AND goal.turn_id = queued.turn_id
       AND goal.goal_generation = retired;
    INSERT INTO session_live_queued_turn (
        session_id, turn_id, acceptance_position
    )
    SELECT lifecycle.session_id, lifecycle.turn_id,
           lifecycle.acceptance_position
      FROM goal_turn AS goal
      JOIN turn_lifecycle AS lifecycle
        ON lifecycle.session_id = goal.session_id
       AND lifecycle.turn_id = goal.turn_id
     WHERE goal.session_id = NEW.session_id
       AND goal.goal_generation = pursued
       AND lifecycle.state_kind = 'queued'
       AND NOT lifecycle.delegation_runtime_terminal
    ON CONFLICT (turn_id) DO NOTHING;
    RETURN NULL;
END
$$;

CREATE TRIGGER goal_event_refreshes_session_live_queue
AFTER INSERT ON goal_event
FOR EACH ROW EXECUTE FUNCTION refresh_session_live_goal_queue();

-- The live projection reads the newest outstanding reconciliation park by its
-- exact terminal shape. Without a shape-keyed index that lookup walks the
-- session's retained terminal history backward, making a bounded current read
-- proportional to lifetime turn count.
CREATE INDEX turn_lifecycle_session_live_reconciliation
    ON turn_lifecycle (session_id, acceptance_position DESC)
    WHERE state_kind = 'terminal'
      AND terminal_disposition_kind = 'reconciliation_required'
      AND NOT delegation_runtime_terminal;
