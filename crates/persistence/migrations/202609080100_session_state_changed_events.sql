CREATE FUNCTION append_session_state_changed_outbox_event() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    allocated numeric(20, 0);
BEGIN
    INSERT INTO outbox_event (event_kind, storage_version, session_id)
    VALUES ('session_state_changed', 1, NEW.session_id)
    RETURNING event_sequence INTO allocated;
    INSERT INTO session_state_changed_outbox_event
        (event_sequence, event_kind, storage_version, session_id,
         prior_state_kind, state_kind, state_entered_at,
         actor_kind, actor_module, actor_turn_id, actor_tool_request_id,
         waiting_kind, waiting_waker, waiting_subject_session_id,
         recovering_op, blocked_reason, blocked_cycle,
         parked_cause, parked_responder, parked_since, parked_standing_cause_kind)
    VALUES
        (allocated, 'session_state_changed', 1, NEW.session_id,
         OLD.state_kind, NEW.state_kind, NEW.state_entered_at,
         NEW.actor_kind, NEW.actor_module, NEW.actor_turn_id, NEW.actor_tool_request_id,
         NEW.waiting_kind, NEW.waiting_waker, NEW.waiting_subject_session_id,
         NEW.recovering_op, NEW.blocked_reason, NEW.blocked_cycle,
         NEW.parked_cause, NEW.parked_responder,
         CASE WHEN NEW.state_kind = 'parked' THEN NEW.parked_since END,
         CASE WHEN NEW.state_kind = 'parked' THEN NEW.parked_standing_cause_kind END);
    RETURN NULL;
END;
$$;

CREATE TRIGGER session_lifecycle_appends_state_changed_outbox_event
    AFTER UPDATE ON session_lifecycle
    FOR EACH ROW
    WHEN (NEW.state_kind <> 'terminal' AND ROW(
        OLD.state_kind, OLD.waiting_kind, OLD.waiting_waker,
        OLD.waiting_subject_session_id, OLD.recovering_op,
        OLD.blocked_reason, OLD.blocked_cycle,
        OLD.parked_cause, OLD.parked_responder, OLD.parked_standing_cause_kind
    ) IS DISTINCT FROM ROW(
        NEW.state_kind, NEW.waiting_kind, NEW.waiting_waker,
        NEW.waiting_subject_session_id, NEW.recovering_op,
        NEW.blocked_reason, NEW.blocked_cycle,
        NEW.parked_cause, NEW.parked_responder, NEW.parked_standing_cause_kind
    ))
    EXECUTE FUNCTION append_session_state_changed_outbox_event();
