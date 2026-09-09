SET ROLE mod_repo_watch;
SET search_path = mod_repo_watch, pg_catalog;

ALTER TABLE dispatch_ledger
    ADD COLUMN kickoff_text text,
    ADD COLUMN kickoff_command_id uuid UNIQUE;

RESET search_path;
RESET ROLE;
