SET ROLE mod_repo_watch;
SET search_path = mod_repo_watch, pg_catalog;

-- growth: one reconciliation cursor per configured repository.
-- retention: removed when its reconciliation completes or reviewers change.
CREATE TABLE poll_cursor (
    repository text PRIMARY KEY,
    cursor jsonb NOT NULL,
    CHECK (jsonb_typeof(cursor) = 'object')
);

RESET search_path;
RESET ROLE;
