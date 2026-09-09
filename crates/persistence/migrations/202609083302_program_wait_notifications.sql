CREATE INDEX program_journal_answer_position
    ON program_run_journal_entry (run_id, journal_position)
    WHERE frame_direction = 'delivery' AND frame_kind = 'answer';

CREATE FUNCTION notify_program_journal_waiters() RETURNS trigger
    LANGUAGE plpgsql AS $$
BEGIN
    PERFORM pg_notify('signalbox_program_journal', NEW.run_id::text);
    RETURN NULL;
END;
$$;

CREATE TRIGGER program_journal_wait_notification
    AFTER INSERT ON program_run_journal_entry
    FOR EACH ROW EXECUTE FUNCTION notify_program_journal_waiters();
