SET ROLE mod_repo_watch;
SET search_path = mod_repo_watch, pg_catalog;

-- growth: one row per observed repository.
-- retention: latest authenticated observation identity.
CREATE TABLE observer_actor (
    repository text PRIMARY KEY,
    login text NOT NULL CHECK (length(login) > 0)
);

ALTER TABLE gh_event ADD COLUMN source_review_actor text;
UPDATE gh_event
   SET source_review_actor = convert_from(normalized_payload, 'UTF8')::jsonb #>> '{kind,reviewer}'
 WHERE event_kind = 'review_submitted' AND decode_error IS NULL;

CREATE OR REPLACE VIEW gh_readable_event AS
SELECT event_id,
       content_identity,
       repository,
       event_kind,
       target_kind,
       pull_request_number,
       normalized_payload,
       recorded_at,
       frontier_generation,
       event_ordinal,
       producer,
       repository_event_ordinal,
       decode_error
  FROM gh_event AS event
 WHERE decode_error IS NULL
   AND NOT EXISTS (
       SELECT 1 FROM observer_actor AS actor
        WHERE actor.repository = event.repository
          AND event.event_kind = 'review_submitted'
          AND actor.login = event.source_review_actor
   )
   AND NOT EXISTS (
       SELECT 1 FROM review_write_receipt AS receipt
        WHERE receipt.repository = event.repository
          AND receipt.review_id = event.source_review_id
   );

RESET search_path;
RESET ROLE;
