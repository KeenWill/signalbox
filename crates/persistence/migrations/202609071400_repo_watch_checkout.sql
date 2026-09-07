SET ROLE mod_repo_watch;
SET search_path = mod_repo_watch, pg_catalog;

ALTER TABLE dispatch_ledger
    ADD COLUMN checkout_path text CHECK (checkout_path = '.'),
    ADD COLUMN checkout_head_sha text,
    ADD COLUMN checkout_retired_reason text CHECK (checkout_retired_reason = 'checkout_provisioning_failed'),
    ADD COLUMN checkout_failure_step text,
    ADD COLUMN checkout_failure_status text,
    ADD COLUMN checkout_stop_command_id uuid,
    ADD CHECK ((checkout_path IS NULL) = (checkout_head_sha IS NULL)),
    ADD CHECK ((checkout_retired_reason IS NULL) = (checkout_failure_step IS NULL)),
    ADD CHECK ((checkout_retired_reason IS NULL) = (checkout_failure_status IS NULL)),
    ADD CHECK ((checkout_retired_reason IS NULL) = (checkout_stop_command_id IS NULL));

RESET search_path;
RESET ROLE;
