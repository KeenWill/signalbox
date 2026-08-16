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

-- Parity causes.
--
-- The rollout gate is no *unexplained* divergence: every webhook-only or
-- poll-only parity row must name a closed cause, and the gate is zero rows
-- without one. Causes a delivery already knows are recorded beside its
-- projection; the poll-only families webhooks are not designed to reproduce are
-- derived, since no delivery exists to carry them.
ALTER TABLE repo_watch_webhook_projection
    ADD COLUMN cause_code text;

ALTER TABLE repo_watch_webhook_projection
    ADD CONSTRAINT repo_watch_webhook_projection_cause_code_check
        CHECK (
            cause_code IS NULL
            OR cause_code IN (
                'compressed_transition',
                'context_drift',
                'poll_only_family',
                'cross_drain_shadow_gap'
            )
        );

-- Supersedes the repo_watch_webhook_parity definition in
-- 202608150002_repo_watch_webhook_intake.sql, which reported a status without a
-- cause. Every other column keeps its meaning and position.
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
     WHERE event.producer = 'poll'
       AND event.content_identity_version = 1
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
