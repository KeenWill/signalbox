-- Admit webhook-produced repository-watch events after the shadow parity gate.

ALTER TABLE repo_watch_event
    DROP CONSTRAINT repo_watch_event_producer_check;

ALTER TABLE repo_watch_event
    ADD CONSTRAINT repo_watch_event_producer_check
        CHECK (producer IN ('poll', 'webhook'));
