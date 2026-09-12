SET ROLE mod_repo_watch;
SET search_path = mod_repo_watch, pg_catalog;

DELETE FROM poll_cursor;

RESET search_path;
RESET ROLE;
