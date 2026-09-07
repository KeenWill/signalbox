CREATE INDEX turn_terminal_outbox_event_latest_failure_idx
    ON turn_terminal_outbox_event (session_id, event_sequence DESC)
    WHERE disposition_kind = 'failed';
