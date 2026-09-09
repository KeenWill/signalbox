SET ROLE mod_repo_watch;
SET search_path = mod_repo_watch, pg_catalog;

ALTER TABLE webhook_pull_wake
    ADD COLUMN failed_attempts integer NOT NULL DEFAULT 0 CHECK (failed_attempts >= 0),
    ADD COLUMN last_failure text;

RESET search_path;
RESET ROLE;
