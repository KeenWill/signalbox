-- Support pull-request target censuses across repository-watch sessions.

CREATE INDEX repo_watch_event_pull_request_target
    ON repo_watch_event (repository, pull_request_number, event_id)
    WHERE target_kind = 'pull_request';

CREATE INDEX repo_watch_dispatch_action_event_target
    ON repo_watch_dispatch_action (
        event_id, recorded_at DESC, dispatch_id DESC, session_id DESC
    );
