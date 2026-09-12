SET ROLE mod_repo_watch;
SET search_path = mod_repo_watch, pg_catalog;

-- growth: one row per confirmed native review write.
-- retention: permanent, so observation replay preserves self-write provenance.
CREATE TABLE review_write_receipt (
    repository text NOT NULL,
    review_id numeric(20,0) NOT NULL,
    comment_id text,
    PRIMARY KEY (repository, review_id),
    CHECK (review_id BETWEEN 1 AND 18446744073709551615)
);

ALTER TABLE gh_event ADD COLUMN source_review_id numeric(20,0);
ALTER TABLE gh_event ADD CONSTRAINT source_review_event_kind
    CHECK (source_review_id IS NULL OR (
        source_review_id BETWEEN 1 AND 18446744073709551615
        AND event_kind IN ('review_submitted', 'thread_opened')
    ));

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
       SELECT 1 FROM review_write_receipt AS receipt
        WHERE receipt.repository = event.repository
          AND receipt.review_id = event.source_review_id
   );

RESET search_path;
RESET ROLE;
