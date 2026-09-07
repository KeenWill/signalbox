SET ROLE mod_repo_watch;
SET search_path = mod_repo_watch, pg_catalog;

CREATE INDEX dispatch_live_session ON dispatch_ledger(command_id)
    WHERE created_session_id IS NOT NULL AND session_terminal_at IS NULL;
CREATE INDEX gh_event_pull_request_terminal
    ON gh_event(repository, pull_request_number, repository_event_ordinal)
    WHERE event_kind IN ('pull_request_closed', 'pull_request_merged');

RESET search_path;
RESET ROLE;
