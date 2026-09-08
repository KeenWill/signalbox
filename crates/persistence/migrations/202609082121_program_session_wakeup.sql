CREATE FUNCTION notify_program_session_activity() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    PERFORM pg_notify('program_session_activity', '');
    RETURN NULL;
END;
$$;
CREATE TRIGGER program_session_turn_terminal
AFTER INSERT OR UPDATE ON turn_lifecycle
FOR EACH ROW WHEN (NEW.state_kind = 'terminal') EXECUTE FUNCTION notify_program_session_activity();
CREATE TRIGGER program_session_run_terminal
AFTER INSERT ON program_run_journal_entry
FOR EACH ROW WHEN (NEW.frame_kind IN ('run_cancel', 'fault')) EXECUTE FUNCTION notify_program_session_activity();
