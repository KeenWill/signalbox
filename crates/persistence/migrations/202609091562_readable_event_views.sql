CREATE VIEW outbox_readable_event AS
SELECT event.event_sequence,
       event.event_kind,
       event.storage_version,
       event.session_id,
       event.turn_disposition,
       event.recorded_at
  FROM outbox_event AS event
 WHERE NOT EXISTS (
    SELECT 1
      FROM outbox_event_quarantine AS quarantine
     WHERE quarantine.event_sequence = event.event_sequence
 )
UNION ALL
SELECT event.event_sequence,
       event.event_kind,
       event.storage_version,
       event.session_id,
       NULL::text AS turn_disposition,
       event.recorded_at
  FROM delegation_outbox_event AS event
 WHERE NOT EXISTS (
    SELECT 1
      FROM outbox_event_quarantine AS quarantine
     WHERE quarantine.event_sequence = event.event_sequence
 );

SET ROLE mod_repo_watch;
SET search_path = mod_repo_watch, pg_catalog;

CREATE VIEW gh_readable_event AS
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
  FROM gh_event
 WHERE decode_error IS NULL;

RESET search_path;
RESET ROLE;
