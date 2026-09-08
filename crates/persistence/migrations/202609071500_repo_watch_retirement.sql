SET ROLE mod_repo_watch;
SET search_path = mod_repo_watch, pg_catalog;

ALTER TABLE dispatch_ledger
    ADD COLUMN session_terminal_at timestamptz,
    ADD COLUMN retirement_event_id uuid REFERENCES gh_event(event_id),
    ADD COLUMN retirement_reason text,
    ADD CONSTRAINT retirement_reason_closed CHECK (
        (retirement_event_id IS NULL AND retirement_reason IS NULL)
        OR (retirement_event_id IS NOT NULL AND retirement_reason IS NOT NULL
            AND retirement_reason IN ('pull_request_closed', 'pull_request_merged')
            AND command_kind = 'lifecycle' AND trigger_sequence IS NULL));

-- The evaluation identity distinguishes core and repository lifecycle triggers.
ALTER TABLE dispatch_ledger DROP CONSTRAINT dispatch_ledger_repository_rule_id_rule_revision_event_id_t_key;
ALTER TABLE dispatch_ledger ADD CONSTRAINT dispatch_evaluation_identity
    UNIQUE NULLS NOT DISTINCT
    (repository, rule_id, rule_revision, event_id, trigger_sequence, retirement_event_id, action_ordinal);
CREATE UNIQUE INDEX dispatch_session_retirement ON dispatch_ledger(dispatch_ref, action_ordinal)
    WHERE retirement_event_id IS NOT NULL;

RESET search_path;
RESET ROLE;
