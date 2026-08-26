-- Bound pull-request operations lookups by their durable event target.

CREATE INDEX repo_watch_event_pull_request_position
    ON repo_watch_event (
        repository,
        pull_request_number,
        cursor_generation DESC,
        event_ordinal DESC
    )
    WHERE pull_request_number IS NOT NULL;
