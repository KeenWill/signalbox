SET ROLE mod_repo_watch;
SET search_path = mod_repo_watch, pg_catalog;

ALTER TABLE repository_state
    ADD COLUMN frontier_generation numeric(20,0) NOT NULL DEFAULT 0,
    ADD COLUMN last_frontier_commit_digest bytea,
    ADD CONSTRAINT repository_state_frontier_generation_u64 CHECK (
        frontier_generation BETWEEN 0 AND 18446744073709551615
    ),
    ADD CONSTRAINT repository_state_frontier_digest_length CHECK (
        last_frontier_commit_digest IS NULL
        OR octet_length(last_frontier_commit_digest) = 32
    ),
    ADD CONSTRAINT repository_state_frontier_commit_pair CHECK (
        (frontier_generation = 0) = (last_frontier_commit_digest IS NULL)
    );

RESET search_path;
RESET ROLE;
