SET ROLE mod_repo_watch;
SET search_path = mod_repo_watch, pg_catalog;

-- The complete canonical differ input is replaced with each frontier commit.
ALTER TABLE repository_state
    ADD COLUMN comparison_baseline jsonb NOT NULL;

RESET search_path;
RESET ROLE;
