SET ROLE mod_repo_watch;
SET search_path = mod_repo_watch, pg_catalog;

-- growth: one pending candidate per unfinished pull request at rule activation.
-- retention: remove after admission or when the current pull request no longer matches.
CREATE TABLE rule_activation_candidate (
    repository text NOT NULL,
    rule_id text NOT NULL,
    rule_revision numeric(20,0) NOT NULL,
    event_id uuid NOT NULL REFERENCES gh_event(event_id),
    PRIMARY KEY (repository, rule_id, rule_revision, event_id),
    FOREIGN KEY (repository, rule_id, rule_revision)
        REFERENCES rule_revision(repository, rule_id, revision) ON DELETE CASCADE
);

ALTER TABLE dispatch_ledger
    ADD COLUMN activation_event bytea,
    DROP CONSTRAINT retry_context_present,
    ADD CONSTRAINT retry_context_present CHECK (
        (retry_of IS NULL) = (retry_event IS NULL) OR
        (retry_of IS NOT NULL AND activation_event IS NOT NULL AND retry_event IS NULL)
    ),
    ADD CONSTRAINT one_reevaluation_context CHECK (
        activation_event IS NULL OR retry_event IS NULL
    );

RESET search_path;
RESET ROLE;
