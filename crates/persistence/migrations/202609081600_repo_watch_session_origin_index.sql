SET ROLE mod_repo_watch;
SET search_path = mod_repo_watch, pg_catalog;

CREATE INDEX dispatch_created_session ON dispatch_ledger(created_session_id);

RESET search_path;
RESET ROLE;
