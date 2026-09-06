SET ROLE mod_repo_watch;
SET search_path = mod_repo_watch, pg_catalog;

ALTER TABLE gh_event
    DROP COLUMN retain_until,
    ADD COLUMN producer text NOT NULL,
    ADD COLUMN repository_event_ordinal numeric(20,0) NOT NULL,
    ADD CONSTRAINT gh_event_producer_check CHECK (
        producer = ANY (ARRAY['poll', 'webhook'])
    ),
    ADD CONSTRAINT gh_event_repository_event_ordinal_u64 CHECK (
        repository_event_ordinal BETWEEN 1 AND 18446744073709551615
    ),
    ADD CONSTRAINT gh_event_repository_event_ordinal_key UNIQUE (
        repository, repository_event_ordinal
    );

ALTER TABLE rule_revision
    ADD COLUMN activated_after_event_ordinal numeric(20,0) NOT NULL,
    ADD CONSTRAINT rule_revision_activation_tail_u64 CHECK (
        activated_after_event_ordinal BETWEEN 0 AND 18446744073709551615
    );

RESET search_path;
RESET ROLE;
