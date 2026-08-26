-- Keep bounded operator session and event-window reads index-seekable.

CREATE INDEX commissioned_dispatch_pull_request_recorded_at
    ON commissioned_dispatch (
        repository,
        pull_request_number,
        recorded_at DESC,
        session_id DESC
    )
    WHERE target_kind = 'pull_request';

CREATE INDEX repo_watch_event_repository_recorded_at
    ON repo_watch_event (repository, recorded_at DESC);
