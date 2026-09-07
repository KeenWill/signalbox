SET ROLE mod_repo_watch;
SET search_path = mod_repo_watch, pg_catalog;

-- Discard PR validators and snapshots whose merge time must be fetched.
DELETE FROM poll_cache_page WHERE snapshot ->> 0 = 'pull';

RESET search_path;
RESET ROLE;
