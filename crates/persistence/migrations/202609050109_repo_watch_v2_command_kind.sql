-- Module dispatch ledgers use the canonical durable-command discriminators.

SET ROLE mod_repo_watch;
SET search_path = mod_repo_watch, pg_catalog;

ALTER TABLE dispatch_ledger
    DROP CONSTRAINT dispatch_ledger_command_kind_check,
    ADD CONSTRAINT dispatch_ledger_command_kind_check CHECK (
        command_kind = ANY (
            ARRAY['create_session', 'submit_input', 'goal', 'session_lifecycle']
        )
    );

RESET search_path;
RESET ROLE;
