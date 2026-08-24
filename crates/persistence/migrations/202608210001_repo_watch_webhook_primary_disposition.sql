-- Restore the committed repository-watch webhook disposition for primary mode.
--
-- 202608170005_repo_watch_webhook_shadow_only_disposition.sql withdrew the
-- committed disposition and its resulting cursor generation while the webhook
-- slice was authorized for shadow mode only. Primary mode is now an implemented
-- contract, so the durable shape that decision deferred is chosen here: a
-- state-changing delivery records `committed` beside the cursor generation its
-- write produced.
--
-- This migration is versioned above 202608170005 deliberately. The primary
-- migration 202608150003_repo_watch_webhook_primary.sql sorts below it, so
-- restoring the column there would be undone by the shadow-only withdrawal on
-- any database that applies both in version order.

ALTER TABLE repo_watch_webhook_disposition
    DROP CONSTRAINT repo_watch_webhook_disposition_shadow_only_check;

-- Restores the column dropped by 202608170005, which also dropped the unnamed
-- CHECK pairing 'committed' with a positive generation in
-- 202608150002_repo_watch_webhook_intake.sql. No row carried a value: only a
-- committed disposition ever could, and none had been recorded.
ALTER TABLE repo_watch_webhook_disposition
    ADD COLUMN resulting_cursor_generation bigint;

-- Re-states the withdrawn pairing under a name, so a later change can address
-- it directly rather than through the column it qualifies.
ALTER TABLE repo_watch_webhook_disposition
    ADD CONSTRAINT repo_watch_webhook_disposition_committed_generation_check
        CHECK (
            (disposition = 'committed' AND resulting_cursor_generation > 0)
            OR (disposition <> 'committed' AND resulting_cursor_generation IS NULL)
        );
