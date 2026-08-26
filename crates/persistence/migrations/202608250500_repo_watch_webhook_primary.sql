-- Admit webhook-produced repository-watch events under primary mode.
--
-- 202608150001_repo_watch_event_content_identity.sql introduced
-- repo_watch_event.producer and constrained it to 'poll', because polling was
-- then the only intake that wrote an ordinary event row. The 2026-08-25 rollout
-- ruling found the webhook parity gate met and authorized primary mode, so an
-- authenticated delivery now commits ordinary rows itself and records the
-- transport that observed them.
--
-- Parity measures the shadow experiment, and the experiment ends for a
-- repository when that repository starts committing deliveries. Reading
-- producer = 'poll' keeps a webhook-produced row off the poll side, but that
-- alone does not end the measurement: the complete reconciliation sweep keeps
-- running under primary mode as the backstop for missed and unmapped facts, and
-- a primary delivery records no event projection for a sweep row to match. Each
-- such row would land as a permanent uncaused poll_only divergence and corrupt
-- the very gate the experiment reports, so the poll side is bounded above by
-- the repository's own promotion.
--
-- The boundary is the repository's first committed delivery, not its first
-- webhook-produced event. Those are not the same moment: an applied delivery is
-- a cursor advance, and a cursor advance derives no event whenever the observed
-- change falls outside the event families. A primary `pull_request: edited`
-- carrying only a new title or body is exactly that — it commits the context
-- and advances the cursor while `derive_pull_request_events` emits nothing. A
-- boundary read from event rows is therefore inert for as long as a repository
-- happens to commit only such deliveries, and every backstop row the sweep
-- produces meanwhile lands as the permanent uncaused poll_only divergence this
-- bound exists to prevent. It does not heal when a later delivery finally does
-- emit an event, because the bound admits rows recorded before it.
--
-- The durable evidence is the terminal disposition the delivery already writes.
-- 202608170005_repo_watch_webhook_shadow_only_disposition.sql withdrew the
-- 'committed' spelling because the write mode had not been decided and
-- reserving it would pre-commit a shape that decision had to be free to choose.
-- This migration is that decision, so the spelling is restored. It records what
-- a delivery did rather than what mode was configured, so a reverted
-- configuration strands nothing, and it exists for every primary commit
-- including the ones that derive no event. It names no resulting cursor
-- generation: 202608170005 already dropped that column, and the two-step
-- handoff records the disposition before the write whose generation it would
-- have to name. Rows the shadow interval already produced keep their
-- classification and their causes, so the measurement that authorized promotion
-- stays readable afterwards.

ALTER TABLE repo_watch_event
    DROP CONSTRAINT repo_watch_event_producer_check;

ALTER TABLE repo_watch_event
    ADD CONSTRAINT repo_watch_event_producer_check
        CHECK (producer IN ('poll', 'webhook'));

-- Restores the sixth disposition 202608170005 withdrew. The unnamed CHECK in
-- 202608150002_repo_watch_webhook_intake.sql still admits it, so dropping the
-- narrowing constraint is the whole change; the resulting_cursor_generation
-- column and its pairing CHECK stay dropped.
ALTER TABLE repo_watch_webhook_disposition
    DROP CONSTRAINT repo_watch_webhook_disposition_shadow_only_check;

-- Supersedes the repo_watch_webhook_parity definition in
-- 202608170005_repo_watch_webhook_shadow_only_disposition.sql, which bounded
-- the poll side below by the first shadow receipt and left it unbounded above.
-- Every column keeps its meaning and position, and only the poll_event source
-- changes.
DROP VIEW repo_watch_webhook_parity;

CREATE VIEW repo_watch_webhook_parity AS
WITH event_projection AS (
    SELECT delivery.repository,
           delivery.hook_id,
           delivery.delivery_id,
           delivery.event_name,
           delivery.action_name,
           delivery.receipt_sequence,
           delivery.received_at,
           projection.projection_ordinal,
           projection.projection_kind,
           projection.event_kind,
           projection.targeted_query_kind,
           projection.targeted_query_key,
           projection.content_identity_version,
           projection.content_identity,
           projection.cause_code,
           projection.projected_at
      FROM repo_watch_webhook_delivery AS delivery
      JOIN repo_watch_webhook_projection AS projection
        ON projection.hook_id = delivery.hook_id
       AND projection.delivery_id = delivery.delivery_id
     WHERE projection.projection_kind = 'event'
),
shadow_start AS (
    SELECT repository, min(received_at) AS started_at
      FROM repo_watch_webhook_delivery
     GROUP BY repository
),
primary_start AS (
    SELECT delivery.repository, min(disposition.recorded_at) AS promoted_at
      FROM repo_watch_webhook_disposition AS disposition
      JOIN repo_watch_webhook_delivery AS delivery
        ON delivery.hook_id = disposition.hook_id
       AND delivery.delivery_id = disposition.delivery_id
     WHERE disposition.disposition = 'committed'
     GROUP BY delivery.repository
),
poll_event AS (
    SELECT event.repository,
           event.event_id,
           event.cursor_generation,
           event.event_ordinal,
           event.event_kind,
           event.content_identity_version,
           event.content_identity,
           event.recorded_at
      FROM repo_watch_event AS event
      JOIN shadow_start AS shadow
        ON shadow.repository = event.repository
       AND event.recorded_at >= shadow.started_at
      LEFT JOIN primary_start AS promotion
        ON promotion.repository = event.repository
     WHERE event.producer = 'poll'
       AND event.content_identity_version = 1
       AND (
           promotion.promoted_at IS NULL
           OR event.recorded_at < promotion.promoted_at
       )
)
SELECT COALESCE(webhook.repository, poll.repository) AS repository,
       webhook.hook_id,
       webhook.delivery_id,
       webhook.event_name,
       webhook.action_name,
       webhook.receipt_sequence,
       webhook.projection_ordinal,
       COALESCE(webhook.projection_kind, 'event') AS projection_kind,
       COALESCE(webhook.event_kind, poll.event_kind) AS projected_event_kind,
       webhook.targeted_query_kind,
       webhook.targeted_query_key,
       COALESCE(
           webhook.content_identity_version,
           poll.content_identity_version
       ) AS content_identity_version,
       COALESCE(webhook.content_identity, poll.content_identity) AS content_identity,
       poll.event_id AS poll_event_id,
       poll.cursor_generation AS poll_cursor_generation,
       poll.event_ordinal AS poll_event_ordinal,
       webhook.received_at,
       webhook.projected_at,
       poll.recorded_at AS poll_recorded_at,
       webhook.projected_at - webhook.received_at AS projection_latency,
       poll.recorded_at - webhook.received_at AS poll_latency,
       CASE
           WHEN webhook.delivery_id IS NOT NULL AND poll.event_id IS NOT NULL
               THEN 'matched'
           WHEN webhook.delivery_id IS NOT NULL THEN 'webhook_only'
           ELSE 'poll_only'
       END AS status,
       CASE
           WHEN webhook.delivery_id IS NOT NULL AND poll.event_id IS NOT NULL THEN NULL
           WHEN webhook.delivery_id IS NOT NULL THEN webhook.cause_code
           WHEN poll.event_kind IN (
               'mergeable_state_changed',
               'checks_completed',
               'reaction_changed'
           ) THEN 'poll_only_family'
           ELSE NULL
       END AS cause
  FROM event_projection AS webhook
  FULL OUTER JOIN poll_event AS poll
    ON poll.content_identity_version = webhook.content_identity_version
   AND poll.content_identity = webhook.content_identity
UNION ALL
SELECT delivery.repository,
       delivery.hook_id,
       delivery.delivery_id,
       delivery.event_name,
       delivery.action_name,
       delivery.receipt_sequence,
       projection.projection_ordinal,
       projection.projection_kind,
       NULL AS projected_event_kind,
       projection.targeted_query_kind,
       projection.targeted_query_key,
       NULL AS content_identity_version,
       NULL AS content_identity,
       NULL AS poll_event_id,
       NULL AS poll_cursor_generation,
       NULL AS poll_event_ordinal,
       delivery.received_at,
       projection.projected_at,
       NULL AS poll_recorded_at,
       projection.projected_at - delivery.received_at AS projection_latency,
       NULL AS poll_latency,
       'not_directly_mapped' AS status,
       NULL AS cause
  FROM repo_watch_webhook_delivery AS delivery
  JOIN repo_watch_webhook_projection AS projection
    ON projection.hook_id = delivery.hook_id
   AND projection.delivery_id = delivery.delivery_id
 WHERE projection.projection_kind = 'targeted_query';
