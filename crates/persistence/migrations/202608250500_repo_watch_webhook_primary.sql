-- Admit webhook-produced repository-watch events under primary mode.
--
-- 202608150001_repo_watch_event_content_identity.sql introduced
-- repo_watch_event.producer and constrained it to 'poll', because polling was
-- then the only intake that wrote an ordinary event row. The owner ruled on
-- 2026-08-25 that the webhook rollout gate is met and that primary mode is
-- authorized, so an authenticated delivery now commits ordinary rows itself and
-- records the transport that observed them.
--
-- The repo_watch_webhook_parity view is deliberately untouched. Its poll side
-- still reads producer = 'poll', so a webhook-produced row never appears there
-- as a poll-only divergence: parity measures the shadow experiment, which ends
-- for a repository at the moment it selects primary mode.

ALTER TABLE repo_watch_event
    DROP CONSTRAINT repo_watch_event_producer_check;

ALTER TABLE repo_watch_event
    ADD CONSTRAINT repo_watch_event_producer_check
        CHECK (producer IN ('poll', 'webhook'));
