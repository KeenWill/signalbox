SET ROLE mod_repo_watch;
SET search_path = mod_repo_watch, pg_catalog;

ALTER TABLE dispatch_ledger
    DROP CONSTRAINT dispatch_ledger_checkout_retired_reason_check,
    ADD CONSTRAINT dispatch_ledger_checkout_retired_reason_check
        CHECK (checkout_retired_reason IN ('checkout_provisioning_failed', 'repository_unconfigured'));

RESET search_path;
RESET ROLE;
