ALTER TABLE program_run_registration ADD COLUMN input bytea NOT NULL;
ALTER TABLE cancel_program_run_command DROP CONSTRAINT cancel_program_run_command_terminal_state_check;
ALTER TABLE cancel_program_run_command ADD CONSTRAINT cancel_program_run_command_terminal_state_check
    CHECK (terminal_state IN ('cancelled', 'faulted', 'succeeded'));

CREATE FUNCTION require_program_success() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    terminal_position numeric;
    outstanding_at_terminal boolean;
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
    IF NEW.resolves_request_ordinal IS NOT NULL THEN
        SELECT journal_position INTO terminal_position FROM program_run_journal_entry
        WHERE run_id = NEW.run_id AND request_ordinal = NEW.resolves_request_ordinal
            AND frame_kind = 'terminal';
        IF terminal_position IS NOT NULL THEN
            SELECT EXISTS (
                SELECT 1 FROM program_run_journal_entry AS request
                WHERE request.run_id = NEW.run_id AND request.frame_direction = 'request'
                    AND request.frame_kind <> 'scope'
                    AND request.journal_position < terminal_position
                    AND NOT EXISTS (
                        SELECT 1 FROM program_run_journal_entry AS delivery
                        WHERE delivery.run_id = request.run_id
                            AND delivery.resolves_request_ordinal = request.request_ordinal
                            AND delivery.journal_position < terminal_position
                    )
            ) INTO outstanding_at_terminal;
            IF NOT (
                (NEW.frame_kind = 'answer' AND NOT outstanding_at_terminal
                    AND NEW.journal_position = terminal_position + 1
                    AND NOT EXISTS (SELECT 1 FROM program_run_journal_entry
                        WHERE run_id = NEW.run_id AND frame_kind IN ('run_cancel', 'fault')))
                OR (NEW.frame_kind = 'reject'
                    AND NEW.reject_reason IS NOT DISTINCT FROM 'outstanding_requests'
                    AND outstanding_at_terminal)
            ) THEN
                RAISE EXCEPTION 'terminal resolution requires an immediate answer without outstanding work or an outstanding-work rejection'
                    USING ERRCODE = '23514';
            END IF;
        END IF;
    END IF;
    RETURN NEW;
END;
$$;
-- The sequence trigger acquires the run lock before this trigger.
CREATE TRIGGER program_run_journal_entry_success BEFORE INSERT ON program_run_journal_entry
    FOR EACH ROW EXECUTE FUNCTION require_program_success();
