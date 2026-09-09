SET ROLE mod_repo_watch;
SET search_path = mod_repo_watch, pg_catalog;

ALTER TABLE dispatch_ledger
    ADD COLUMN kickoff_text text,
    ADD COLUMN kickoff_command_id uuid UNIQUE,
    ADD CHECK (kickoff_command_id IS NULL OR
        (kickoff_text IS NOT NULL AND checkout_head_sha IS NOT NULL));

RESET search_path;
RESET ROLE;
