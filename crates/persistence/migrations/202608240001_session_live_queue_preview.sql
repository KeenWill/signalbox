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

-- Goal events change which queued turns count as pursued work, so this
-- rebuilds the session's rows from the one predicate the backfill above
-- uses rather than applying a per-kind delta. Relevance is decided by the
-- session's single latest goal event: `blocked` and `achieved` retire a
-- generation's queued turns exactly as `user_stopped` and `superseded` do,
-- and a later `resumed` restores them, so a delta keyed on event kind reads
-- high against a blocked or achieved generation and a live read then sees
-- more preview rows than `session_timeline_fact` counts, which fails closed.
-- This is the same reasoning `reconcile_session_timeline_goal_work_fact`
-- records for the count itself.
CREATE FUNCTION refresh_session_live_goal_queue()
RETURNS trigger
LANGUAGE plpgsql
SET search_path FROM CURRENT AS $$
BEGIN
    -- Same allocator-then-rows lock order as refresh_session_live_queued_turn;
    -- the goal-event fact trigger that runs before this one already holds it.
    PERFORM 1 FROM outbox_sequence_state WHERE singleton FOR UPDATE;
    DELETE FROM session_live_queued_turn
     WHERE session_id = NEW.session_id;
    INSERT INTO session_live_queued_turn (
        session_id, turn_id, acceptance_position
    )
    SELECT session_id, turn_id, acceptance_position
      FROM turn_lifecycle
     WHERE session_id = NEW.session_id
       AND state_kind = 'queued'
       AND NOT delegation_runtime_terminal
       AND goal_turn_is_runtime_relevant(session_id, turn_id);
    RETURN NULL;
END
$$;

CREATE TRIGGER goal_event_refreshes_session_live_queue
AFTER INSERT ON goal_event
FOR EACH ROW EXECUTE FUNCTION refresh_session_live_goal_queue();
