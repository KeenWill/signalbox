-- Durable record of when each repository last completed a full provider sweep.
--
-- The poll deadline a restarting daemon installs was anchored on its own
-- process start, so a daemon restarting more often than a repository's interval
-- reset the deadline every time and the authoritative completeness sweep never
-- came due. The cursor cannot answer this on its own: a targeted webhook
-- refresh commits generations too, and an unchanged sweep commits none at all,
-- so neither the newest generation's age nor its existence measures the sweep
-- cadence. One row per repository, rewritten by every commit that carries
-- convergence assessments — which is exactly the complete poll's commit — is
-- what the restart reads.

CREATE TABLE repo_watch_complete_poll (
    repository text PRIMARY KEY,
    completed_at timestamptz NOT NULL DEFAULT transaction_timestamp(),

    CHECK (repo_watch_repository_is_valid(repository))
);
