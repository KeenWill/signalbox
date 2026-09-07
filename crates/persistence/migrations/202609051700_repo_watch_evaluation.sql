SET ROLE mod_repo_watch;
SET search_path = mod_repo_watch, pg_catalog;

-- growth: one mutable evaluation position per configured rule revision.
-- retention: delete with a releasable inactive revision.
CREATE TABLE rule_evaluation_cursor (
    repository text NOT NULL,
    rule_id text NOT NULL,
    rule_revision numeric(20,0) NOT NULL,
    event_ordinal numeric(20,0) NOT NULL,
    PRIMARY KEY (repository, rule_id, rule_revision),
    FOREIGN KEY (repository, rule_id, rule_revision)
        REFERENCES rule_revision(repository, rule_id, revision) ON DELETE CASCADE,
    CHECK (event_ordinal BETWEEN 0 AND 18446744073709551615)
);

ALTER TABLE dispatch_ledger
    ADD COLUMN singleton_key text,
    ADD COLUMN singleton_released_at timestamptz,
    ADD COLUMN submission_pending boolean NOT NULL DEFAULT false;

CREATE INDEX dispatch_singleton ON dispatch_ledger (rule_id, rule_revision, singleton_key)
    WHERE singleton_key IS NOT NULL;

RESET search_path;
RESET ROLE;
