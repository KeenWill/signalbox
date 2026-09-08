SET ROLE mod_repo_watch;
SET search_path = mod_repo_watch, pg_catalog;

ALTER TABLE dispatch_ledger
    ADD COLUMN checkout_removed boolean NOT NULL DEFAULT false,
    ADD COLUMN checkout_created boolean NOT NULL DEFAULT false,
    ADD COLUMN checkout_workspace_root bytea,
    ADD COLUMN checkout_session_id uuid,
    ADD COLUMN checkout_device numeric(20, 0),
    ADD COLUMN checkout_inode numeric(20, 0),
    ADD CHECK ((checkout_workspace_root IS NULL) = (checkout_session_id IS NULL)),
    ADD CHECK ((checkout_device IS NULL) = (checkout_inode IS NULL));

UPDATE dispatch_ledger SET checkout_removed = true WHERE checkout_path IS NOT NULL;

ALTER TABLE dispatch_ledger
    ADD CHECK (checkout_path IS NULL OR checkout_session_id IS NOT NULL OR checkout_removed);

RESET search_path;
RESET ROLE;
