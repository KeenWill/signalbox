-- Shadow-only repository-watch webhook dispositions.
--
-- The webhook slice is authorized for shadow mode only: the runtime never
-- produces a committed disposition, and the implemented contract requires any
-- later write mode to be separately reviewed. Reserving a committed state and
-- its resulting cursor generation would pre-commit the durable shape that
-- decision has to be free to choose, so both are withdrawn until it is made.

-- Dropping the column also drops the unnamed CHECK pairing 'committed' with a
-- positive resulting_cursor_generation in
-- 202608150002_repo_watch_webhook_intake.sql. No row sets it: only a committed
-- disposition ever could, and none has been recorded.
ALTER TABLE repo_watch_webhook_disposition
    DROP COLUMN resulting_cursor_generation;

-- Narrows, rather than supersedes, the unnamed disposition CHECK in
-- 202608150002_repo_watch_webhook_intake.sql: that constraint still admits the
-- five shadow dispositions, and this one withdraws the sixth.
ALTER TABLE repo_watch_webhook_disposition
    ADD CONSTRAINT repo_watch_webhook_disposition_shadow_only_check
        CHECK (disposition <> 'committed');
