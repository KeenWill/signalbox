ALTER TABLE program_run_registration ADD COLUMN input bytea NOT NULL;
ALTER TABLE cancel_program_run_command DROP CONSTRAINT cancel_program_run_command_terminal_state_check;
ALTER TABLE cancel_program_run_command ADD CONSTRAINT cancel_program_run_command_terminal_state_check
    CHECK (terminal_state IN ('cancelled', 'faulted', 'succeeded'));

CREATE FUNCTION require_program_success() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE terminal_position numeric;
BEGIN
    IF EXISTS (
        SELECT 1 FROM program_run_journal_entry AS answer
        JOIN program_run_journal_entry AS request ON request.run_id = answer.run_id
            AND request.request_ordinal = answer.resolves_request_ordinal
        WHERE answer.run_id = NEW.run_id AND answer.frame_kind = 'answer'
            AND request.frame_kind = 'terminal'
    ) THEN
        RAISE EXCEPTION 'journal frame follows accepted success' USING ERRCODE = '23514';
    END IF;
    IF NEW.frame_kind = 'answer' THEN
        SELECT journal_position INTO terminal_position FROM program_run_journal_entry
        WHERE run_id = NEW.run_id AND request_ordinal = NEW.resolves_request_ordinal
            AND frame_kind = 'terminal';
        IF terminal_position IS NOT NULL AND (
            EXISTS (SELECT 1 FROM program_run_journal_entry
                WHERE run_id = NEW.run_id AND frame_kind IN ('run_cancel', 'fault'))
            OR EXISTS (
                SELECT 1 FROM program_run_journal_entry AS request
                WHERE request.run_id = NEW.run_id AND request.frame_direction = 'request'
                    AND request.frame_kind <> 'scope'
                    AND request.request_ordinal <> NEW.resolves_request_ordinal
                    AND (NOT EXISTS (
                        SELECT 1 FROM program_run_journal_entry AS delivery
                        WHERE delivery.run_id = request.run_id
                            AND delivery.resolves_request_ordinal = request.request_ordinal
                    ) OR (request.journal_position < terminal_position AND NOT EXISTS (
                        SELECT 1 FROM program_run_journal_entry AS delivery
                        WHERE delivery.run_id = request.run_id
                            AND delivery.resolves_request_ordinal = request.request_ordinal
                            AND delivery.journal_position < terminal_position
                    )))
            )
        ) THEN
            RAISE EXCEPTION 'terminal answer requires a running run with no other outstanding requests'
                USING ERRCODE = '23514';
        END IF;
    END IF;
    RETURN NEW;
END;
$$;
-- The sequence trigger acquires the run lock before this trigger.
CREATE TRIGGER program_run_journal_entry_success BEFORE INSERT ON program_run_journal_entry
    FOR EACH ROW EXECUTE FUNCTION require_program_success();
