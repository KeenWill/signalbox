-- Shared content identity for repository-watch events produced after this migration.

DROP TRIGGER repo_watch_cursor_is_append_only ON repo_watch_cursor;

ALTER TABLE repo_watch_cursor
    DROP CONSTRAINT repo_watch_cursor_storage_version_check;

UPDATE repo_watch_cursor
   SET storage_version = 2,
       cursor_payload = jsonb_set(
           jsonb_set(cursor_payload, '{storage_version}', '2'::jsonb, false),
           '{event_identity_frontier}',
           '[]'::jsonb,
           true
       );

ALTER TABLE repo_watch_cursor
    ADD CONSTRAINT repo_watch_cursor_storage_version_check
        CHECK (storage_version = 2);

CREATE TRIGGER repo_watch_cursor_is_append_only
BEFORE UPDATE OR DELETE ON repo_watch_cursor
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

ALTER TABLE repo_watch_event
    ADD COLUMN content_identity_version smallint,
    ADD COLUMN content_identity bytea,
    ADD COLUMN producer text;

DROP TRIGGER repo_watch_event_is_append_only ON repo_watch_event;

UPDATE repo_watch_event
   SET content_identity_version = 0,
       content_identity = sha256(
           convert_to(
               'signalbox/repo-watch/legacy-event-identity/v0',
               'UTF8'
           ) || uuid_send(event_id)
       ),
       producer = 'poll';

ALTER TABLE repo_watch_event
    ALTER COLUMN content_identity_version SET NOT NULL,
    ALTER COLUMN content_identity SET NOT NULL,
    ALTER COLUMN producer SET NOT NULL,
    ADD CONSTRAINT repo_watch_event_content_identity_version_check
        CHECK (content_identity_version IN (0, 1)),
    ADD CONSTRAINT repo_watch_event_content_identity_length_check
        CHECK (octet_length(content_identity) = 32),
    ADD CONSTRAINT repo_watch_event_producer_check
        CHECK (producer = 'poll'),
    ADD CONSTRAINT repo_watch_event_content_identity_key
        UNIQUE (content_identity_version, content_identity);

CREATE TRIGGER repo_watch_event_is_append_only
BEFORE UPDATE OR DELETE ON repo_watch_event
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();
