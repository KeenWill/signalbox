SET ROLE mod_repo_watch;
SET search_path = mod_repo_watch, pg_catalog;

ALTER TABLE dispatch_ledger
    ADD COLUMN retry_of uuid,
    ADD COLUMN retry_event bytea,
    ADD CONSTRAINT retry_context_present CHECK ((retry_of IS NULL) = (retry_event IS NULL));
ALTER TABLE dispatch_ledger DROP CONSTRAINT dispatch_evaluation_identity;
ALTER TABLE dispatch_ledger ADD CONSTRAINT dispatch_evaluation_identity
    UNIQUE NULLS NOT DISTINCT
    (repository, rule_id, rule_revision, event_id, trigger_sequence, retirement_event_id, retry_of, action_ordinal);

RESET search_path;
RESET ROLE;
