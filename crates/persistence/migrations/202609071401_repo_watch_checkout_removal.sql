SET ROLE mod_repo_watch;
SET search_path = mod_repo_watch, pg_catalog;

ALTER TABLE dispatch_ledger
    ADD COLUMN checkout_removed boolean NOT NULL DEFAULT false,
    ADD COLUMN checkout_workspace_root bytea,
    ADD COLUMN checkout_session_id uuid,
    ADD CHECK ((checkout_workspace_root IS NULL) = (checkout_session_id IS NULL));

RESET search_path;
RESET ROLE;
