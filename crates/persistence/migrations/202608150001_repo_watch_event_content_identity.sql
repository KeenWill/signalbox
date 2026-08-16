-- Shared content identity for repository-watch events.
--
-- Repository-watch events gain a source-independent content identity, a
-- producer discriminator, and the occurrence frontier the cursor carries.
-- Exactly one identity version is readable after this migration: the database
-- is carried directly to version 1, and no earlier event shape survives for a
-- decoder to admit.

DROP TRIGGER repo_watch_cursor_is_append_only ON repo_watch_cursor;

-- Supersedes the repo_watch_cursor_storage_version_check definition in
-- 202608030002_repo_watch.sql, which pinned the cursor payload at storage
-- version 1, before the payload carried an occurrence-identity frontier.
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

-- Events recorded before this migration predate content identity, and the
-- frontier reset above discards the sequence state their identities would be
-- derived from. They are carried to version 1 under a hash domain reserved for
-- this migration, disjoint from the differ's, so a carried row can never claim
-- an identity the differ would also produce. Dependent dispatch rows reference
-- these events under ON DELETE RESTRICT, so the rows are carried rather than
-- discarded. This is the one-time carry the pre-alpha rule admits; it leaves no
-- second shape for any reader to accept.
UPDATE repo_watch_event
   SET content_identity_version = 1,
       content_identity = sha256(
           convert_to(
               'signalbox/repo-watch/migrated-event-identity/v1',
               'UTF8'
           ) || uuid_send(event_id)
       ),
       producer = 'poll';

ALTER TABLE repo_watch_event
    ALTER COLUMN content_identity_version SET NOT NULL,
    ALTER COLUMN content_identity SET NOT NULL,
    ALTER COLUMN producer SET NOT NULL,
    ADD CONSTRAINT repo_watch_event_content_identity_version_check
        CHECK (content_identity_version = 1),
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
