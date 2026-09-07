SET ROLE mod_repo_watch;
SET search_path = mod_repo_watch, pg_catalog;

-- growth: one reviewer set per polled repository.
-- retention: retained across stops and restarts.
CREATE TABLE poll_cache_reviewers (
    repository text PRIMARY KEY,
    reviewers jsonb NOT NULL CHECK (jsonb_typeof(reviewers) = 'array')
);

-- growth: one snapshot per bounded canonical REST resource or page key.
-- retention: replaced on accepted polls; cleared on reviewer-set changes.
CREATE TABLE poll_cache_page (
    repository text NOT NULL REFERENCES poll_cache_reviewers(repository),
    resource_key text NOT NULL,
    etag text,
    last_modified text,
    has_next boolean NOT NULL,
    snapshot jsonb NOT NULL CHECK (jsonb_typeof(snapshot) = 'array'),
    PRIMARY KEY (repository, resource_key),
    CHECK (etag IS NOT NULL OR last_modified IS NOT NULL)
);
RESET ROLE;
SET search_path = public;
