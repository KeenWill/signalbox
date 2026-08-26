-- Storage version four: merged pull requests gain a compact comparison baseline.
--
-- Existing version-three cursors retain their full pull-request observations
-- and begin with no compact baselines. The first successful cursor commit on
-- the new runtime moves each merged observation into the compact collection,
-- preserving the prior state needed to recognize recurring post-merge events.
-- Seeding an empty collection here therefore changes only the durable shape;
-- it neither drops history nor rewrites the cursor by hand.

DROP TRIGGER repo_watch_cursor_is_append_only ON repo_watch_cursor;

ALTER TABLE repo_watch_cursor
    DROP CONSTRAINT repo_watch_cursor_storage_version_check;

UPDATE repo_watch_cursor
   SET storage_version = 4,
       cursor_payload = jsonb_set(
           jsonb_set(cursor_payload, '{storage_version}', '4'::jsonb, false),
           '{merged_pull_request_baselines}',
           '[]'::jsonb,
           true
       );

ALTER TABLE repo_watch_cursor
    ADD CONSTRAINT repo_watch_cursor_storage_version_check
        CHECK (storage_version = 4);

CREATE TRIGGER repo_watch_cursor_is_append_only
BEFORE UPDATE OR DELETE ON repo_watch_cursor
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();
