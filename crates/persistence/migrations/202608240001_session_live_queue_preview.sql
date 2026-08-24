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

CREATE FUNCTION refresh_session_live_goal_queue()
RETURNS trigger
LANGUAGE plpgsql
SET search_path FROM CURRENT AS $$
BEGIN
    IF NEW.event_kind IN ('user_stopped', 'superseded') THEN
        DELETE FROM session_live_queued_turn AS queued
         USING goal_turn AS goal
         WHERE queued.session_id = NEW.session_id
           AND goal.session_id = queued.session_id
           AND goal.turn_id = queued.turn_id
           AND goal.goal_generation = NEW.generation;
    ELSIF NEW.event_kind IN ('commissioned', 'resumed') THEN
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
           AND goal.goal_generation = NEW.generation
           AND lifecycle.state_kind = 'queued'
           AND NOT lifecycle.delegation_runtime_terminal
        ON CONFLICT (turn_id) DO NOTHING;
    END IF;
    RETURN NULL;
END
$$;

CREATE TRIGGER goal_event_refreshes_session_live_queue
AFTER INSERT ON goal_event
FOR EACH ROW EXECUTE FUNCTION refresh_session_live_goal_queue();
