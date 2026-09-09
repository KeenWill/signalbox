CREATE TABLE goal_stop_settlement (
    session_id uuid NOT NULL,
    event_ordinal numeric NOT NULL,
    turn_id uuid,
    defaults_version numeric NOT NULL,
    interrupt_command_id uuid NOT NULL UNIQUE,
    abandoned_actions bigint CHECK (abandoned_actions >= 0),
    PRIMARY KEY (session_id, event_ordinal),
    FOREIGN KEY (session_id, event_ordinal)
        REFERENCES goal_event(session_id, event_ordinal),
    FOREIGN KEY (turn_id, session_id)
        REFERENCES turn_lifecycle(turn_id, session_id),
    CHECK (turn_id IS NOT NULL OR abandoned_actions = 0)
);

CREATE FUNCTION record_goal_stop_settlement() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
    live_turn uuid;
BEGIN
    IF NEW.event_kind <> 'user_stopped' THEN
        RETURN NULL;
    END IF;
    SELECT turn_id INTO live_turn FROM turn_lifecycle
     WHERE session_id = NEW.session_id AND state_kind = 'active'
       AND NOT delegation_runtime_terminal;
    INSERT INTO goal_stop_settlement
        (session_id, event_ordinal, turn_id, defaults_version,
         interrupt_command_id, abandoned_actions)
    SELECT NEW.session_id, NEW.event_ordinal, live_turn, current_version,
           gen_random_uuid(), CASE WHEN live_turn IS NULL THEN 0 END
      FROM session_current_defaults WHERE session_id = NEW.session_id;
    RETURN NULL;
END;
$$;

CREATE TRIGGER goal_event_records_stop_settlement
AFTER INSERT ON goal_event
FOR EACH ROW EXECUTE FUNCTION record_goal_stop_settlement();

CREATE FUNCTION settle_goal_stop_from_turn() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.state_kind <> 'terminal' THEN
        RETURN NULL;
    END IF;
    UPDATE goal_stop_settlement AS stop
       SET abandoned_actions = (
           SELECT count(*) FROM tool_request AS request
           JOIN tool_approval_decision AS approval USING (request_id)
           WHERE request.turn_id = NEW.turn_id
             AND approval.decision_kind = 'approve'
             AND (EXISTS (
                 SELECT 1 FROM semantic_transcript_entry AS entry
                  WHERE entry.source_session_id = NEW.session_id
                    AND entry.payload_kind = 'tool_closed_by_turn_end'
                    AND entry.tool_result_request_id = request.request_id
             ) AND NOT EXISTS (
                 SELECT 1 FROM tool_attempt AS attempt
                  WHERE attempt.request_id = request.request_id
             ) OR EXISTS (
                 SELECT 1 FROM tool_attempt AS attempt
                  WHERE attempt.request_id = request.request_id
                    AND attempt.error_kind = 'preauthorization_rejected'
                    AND attempt.error_detail = 'goal_stopped'
             ))
       )
     WHERE stop.session_id = NEW.session_id AND stop.turn_id = NEW.turn_id
       AND stop.abandoned_actions IS NULL;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER turn_lifecycle_settles_goal_stop
AFTER INSERT OR UPDATE ON turn_lifecycle
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION settle_goal_stop_from_turn();

CREATE FUNCTION goal_turn_is_scheduling_relevant(checked_session uuid, checked_turn uuid)
RETURNS boolean LANGUAGE sql STABLE AS $$
    SELECT goal_turn_is_runtime_relevant(checked_session, checked_turn)
       OR EXISTS (
            SELECT 1 FROM goal_stop_settlement AS stop
            JOIN accepted_input AS accepted
              ON accepted.accepting_command_id = stop.interrupt_command_id
            WHERE accepted.session_id = checked_session
              AND accepted.origin_turn_id = checked_turn
       );
$$;
