-- Durable GitHub webhook intake and shadow-mode parity for repository watch.

CREATE TABLE repo_watch_webhook_delivery (
    hook_id numeric(20, 0) NOT NULL,
    delivery_id uuid NOT NULL,
    repository text NOT NULL,
    event_name text NOT NULL,
    action_name text,
    body_digest bytea NOT NULL,
    receipt_sequence bigint GENERATED ALWAYS AS IDENTITY,
    received_at timestamptz NOT NULL DEFAULT transaction_timestamp(),

    PRIMARY KEY (hook_id, delivery_id),
    UNIQUE (receipt_sequence),
    CHECK (hook_id > 0 AND hook_id <= 18446744073709551615),
    CHECK (repo_watch_repository_is_valid(repository)),
    CHECK (
        octet_length(event_name) BETWEEN 1 AND 64
        AND event_name COLLATE "C" ~ '^[a-z0-9_]+$'
    ),
    CHECK (
        action_name IS NULL
        OR (
            octet_length(action_name) BETWEEN 1 AND 64
            AND action_name COLLATE "C" ~ '^[a-z0-9_]+$'
        )
    ),
    CHECK (octet_length(body_digest) = 32),
    CHECK (receipt_sequence > 0)
);

CREATE TABLE repo_watch_webhook_payload (
    hook_id numeric(20, 0) NOT NULL,
    delivery_id uuid NOT NULL,
    body bytea NOT NULL,

    PRIMARY KEY (hook_id, delivery_id),
    FOREIGN KEY (hook_id, delivery_id)
        REFERENCES repo_watch_webhook_delivery(hook_id, delivery_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CHECK (octet_length(body) > 0)
);

CREATE TABLE repo_watch_webhook_projection (
    hook_id numeric(20, 0) NOT NULL,
    delivery_id uuid NOT NULL,
    projection_ordinal integer NOT NULL,
    projection_kind text NOT NULL,
    content_identity_version smallint,
    content_identity bytea,
    event_kind text,
    targeted_query_kind text,
    targeted_query_key text,
    occurrence_key bytea,
    projected_at timestamptz NOT NULL DEFAULT transaction_timestamp(),

    PRIMARY KEY (hook_id, delivery_id, projection_ordinal),
    FOREIGN KEY (hook_id, delivery_id)
        REFERENCES repo_watch_webhook_delivery(hook_id, delivery_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CHECK (projection_ordinal > 0),
    CHECK (projection_kind IN ('event', 'targeted_query')),
    CHECK (content_identity_version IS NULL OR content_identity_version = 1),
    CHECK (content_identity IS NULL OR octet_length(content_identity) = 32),
    CHECK (
        event_kind IS NULL
        OR event_kind IN (
            'pull_request_opened',
            'pull_request_closed',
            'pull_request_merged',
            'head_changed',
            'mergeable_state_changed',
            'checks_completed',
            'check_run_completed',
            'branch_workflow_run_completed',
            'review_submitted',
            'thread_opened',
            'thread_resolved',
            'labeled',
            'unlabeled',
            'base_advanced',
            'reaction_changed'
        )
    ),
    CHECK (
        targeted_query_kind IS NULL
        OR targeted_query_kind IN (
            'pull_request_hydration', 'mergeability', 'check_rollup'
        )
    ),
    CHECK (
        targeted_query_key IS NULL
        OR octet_length(targeted_query_key) BETWEEN 1 AND 256
    ),
    CHECK (
        targeted_query_kind IS NULL
        OR (
            targeted_query_kind IN ('pull_request_hydration', 'mergeability')
            AND targeted_query_key COLLATE "C" ~ '^[1-9][0-9]*$'
            AND targeted_query_key::numeric <= 18446744073709551615
        )
        OR (
            targeted_query_kind = 'check_rollup'
            AND targeted_query_key COLLATE "C" ~ '^[0-9a-f]{40}$'
        )
    ),
    CHECK (occurrence_key IS NULL OR octet_length(occurrence_key) > 0),
    CHECK (
        (
            projection_kind = 'event'
            AND content_identity_version = 1
            AND content_identity IS NOT NULL
            AND event_kind IS NOT NULL
            AND targeted_query_kind IS NULL
            AND targeted_query_key IS NULL
            AND occurrence_key IS NOT NULL
        )
        OR (
            projection_kind = 'targeted_query'
            AND content_identity_version IS NULL
            AND content_identity IS NULL
            AND event_kind IS NULL
            AND targeted_query_kind IS NOT NULL
            AND targeted_query_key IS NOT NULL
            AND occurrence_key IS NULL
        )
    )
);

CREATE TABLE repo_watch_webhook_disposition (
    hook_id numeric(20, 0) NOT NULL,
    delivery_id uuid NOT NULL,
    disposition text NOT NULL,
    outcome_code text,
    resulting_cursor_generation bigint,
    recorded_at timestamptz NOT NULL DEFAULT transaction_timestamp(),

    PRIMARY KEY (hook_id, delivery_id),
    FOREIGN KEY (hook_id, delivery_id)
        REFERENCES repo_watch_webhook_delivery(hook_id, delivery_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CHECK (
        disposition IN (
            'projected',
            'committed',
            'duplicate_state',
            'superseded',
            'ignored',
            'quarantined'
        )
    ),
    CHECK (
        outcome_code IS NULL
        OR (
            octet_length(outcome_code) BETWEEN 1 AND 64
            AND outcome_code COLLATE "C" ~ '^[a-z0-9_]+$'
        )
    ),
    CHECK (
        (disposition = 'committed' AND resulting_cursor_generation > 0)
        OR (disposition <> 'committed' AND resulting_cursor_generation IS NULL)
    )
);

CREATE INDEX repo_watch_webhook_delivery_pending_order
    ON repo_watch_webhook_delivery(repository, receipt_sequence);

CREATE INDEX repo_watch_webhook_projection_content_identity
    ON repo_watch_webhook_projection(content_identity_version, content_identity)
    WHERE projection_kind = 'event';

CREATE FUNCTION retain_repo_watch_webhook_payload_until_expired()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF TG_OP = 'UPDATE' THEN
        RAISE EXCEPTION 'repo_watch_webhook_payload is append-only'
            USING ERRCODE = '23514';
    END IF;
    IF NOT EXISTS (
        SELECT 1
          FROM repo_watch_webhook_delivery AS delivery
          JOIN repo_watch_webhook_disposition AS disposition
            ON disposition.hook_id = delivery.hook_id
           AND disposition.delivery_id = delivery.delivery_id
         WHERE delivery.hook_id = OLD.hook_id
           AND delivery.delivery_id = OLD.delivery_id
           AND disposition.recorded_at <= statement_timestamp() - interval '7 days'
    ) THEN
        RAISE EXCEPTION
            'repo-watch webhook payload is not terminal and seven days old'
            USING ERRCODE = '23514';
    END IF;
    RETURN OLD;
END;
$$;

CREATE TRIGGER repo_watch_webhook_delivery_is_append_only
BEFORE UPDATE OR DELETE ON repo_watch_webhook_delivery
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER repo_watch_webhook_payload_retains_unexpired
BEFORE UPDATE OR DELETE ON repo_watch_webhook_payload
FOR EACH ROW
EXECUTE FUNCTION retain_repo_watch_webhook_payload_until_expired();

CREATE TRIGGER repo_watch_webhook_projection_is_append_only
BEFORE UPDATE OR DELETE ON repo_watch_webhook_projection
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER repo_watch_webhook_disposition_is_append_only
BEFORE UPDATE OR DELETE ON repo_watch_webhook_disposition
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER repo_watch_webhook_delivery_reject_truncate
BEFORE TRUNCATE ON repo_watch_webhook_delivery
FOR EACH STATEMENT
EXECUTE FUNCTION reject_repo_watch_table_truncate();

CREATE TRIGGER repo_watch_webhook_payload_reject_truncate
BEFORE TRUNCATE ON repo_watch_webhook_payload
FOR EACH STATEMENT
EXECUTE FUNCTION reject_repo_watch_table_truncate();

CREATE TRIGGER repo_watch_webhook_projection_reject_truncate
BEFORE TRUNCATE ON repo_watch_webhook_projection
FOR EACH STATEMENT
EXECUTE FUNCTION reject_repo_watch_table_truncate();

CREATE TRIGGER repo_watch_webhook_disposition_reject_truncate
BEFORE TRUNCATE ON repo_watch_webhook_disposition
FOR EACH STATEMENT
EXECUTE FUNCTION reject_repo_watch_table_truncate();

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
       END AS status
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
       'not_directly_mapped' AS status
  FROM repo_watch_webhook_delivery AS delivery
  JOIN repo_watch_webhook_projection AS projection
    ON projection.hook_id = delivery.hook_id
   AND projection.delivery_id = delivery.delivery_id
 WHERE projection.projection_kind = 'targeted_query';
