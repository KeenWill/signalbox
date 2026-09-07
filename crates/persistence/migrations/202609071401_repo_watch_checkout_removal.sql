SET ROLE mod_repo_watch;
SET search_path = mod_repo_watch, pg_catalog;

ALTER TABLE dispatch_ledger
    ADD COLUMN checkout_removed boolean NOT NULL DEFAULT false;

RESET search_path;
RESET ROLE;
