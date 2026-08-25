-- Storage version three: a recurring stream records the pull request owning it.
--
-- A frontier entry now carries the pull request that owns its stream, and a
-- repository-global stream stores null. Nothing reads that member yet. It is
-- recorded now because it cannot be recovered later: a stream identity is a
-- one-way domain-separated hash, so no query can derive the pull request a
-- stored 32-byte identity came from. No lifecycle releases a stream, and none
-- may be added without first deciding which subject provably produces no
-- further occurrence — a merged pull request is not one, since its labels
-- change after merge and a completed check run's conclusion can change under an
-- unchanged run identity and completion generation.
--
-- The live frontier is reset rather than migrated. Decoding a version-two entry
-- as unowned would be version-tolerant decoding, which AGENTS.md forbids under
-- pre-alpha compatibility, and the same one-way hash rules out reconstructing
-- ownership from what version two stored.
--
-- The cost is one repeat identification pass. Recurring streams restart at
-- sequence one, so the next occurrence on a stream that already produced events
-- mints a content identity a durable row may already hold. That is the same
-- cost 202608150001_repo_watch_event_content_identity.sql took when it reset
-- the frontier for storage version two, and it is bounded: it is paid once per
-- repository, on the first poll after this migration.

DROP TRIGGER repo_watch_cursor_is_append_only ON repo_watch_cursor;

ALTER TABLE repo_watch_cursor
    DROP CONSTRAINT repo_watch_cursor_storage_version_check;

UPDATE repo_watch_cursor
   SET storage_version = 3,
       cursor_payload = jsonb_set(
           jsonb_set(cursor_payload, '{storage_version}', '3'::jsonb, false),
           '{event_identity_frontier}',
           '[]'::jsonb,
           true
       );

ALTER TABLE repo_watch_cursor
    ADD CONSTRAINT repo_watch_cursor_storage_version_check
        CHECK (storage_version = 3);

CREATE TRIGGER repo_watch_cursor_is_append_only
BEFORE UPDATE OR DELETE ON repo_watch_cursor
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();
