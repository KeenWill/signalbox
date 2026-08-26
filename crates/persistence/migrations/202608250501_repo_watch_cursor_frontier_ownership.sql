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
-- Every carried entry keeps its stream identity and its sequence. Replacing the
-- frontier with an empty one would restart every recurring stream at sequence
-- one, and the next occurrence on a stream that already produced events would
-- then mint a content identity a durable row already holds; the commit
-- coalesces exactly that occurrence, so the event and every dispatch it would
-- have caused are lost without a trace, once per pre-migration occurrence the
-- stream repeats under unchanged identified content.
--
-- 202608150001_repo_watch_event_content_identity.sql is not a precedent for
-- paying that cost. It stamped every row that existed then with
-- content-identity version zero and minted version one alone from then on, and
-- coalescing searches version one, so nothing its reset could collide with
-- existed. This migration has no such separation: the version-one rows a reset
-- would collide with are the ones already stored.
--
-- A carried entry names no owning pull request, because version two stored
-- none and the one-way hash cannot recover one. Writing that member here is
-- the one-time migration carrying a live database across a shape change, not
-- the version-tolerant decoding AGENTS.md forbids under pre-alpha
-- compatibility: the decoder still refuses an entry that omits it. A carried
-- stream's next occurrence overwrites the member with the pull request that
-- produced it, so ownership is accurate from the first advance onward and only
-- a stream that never recurs again keeps null. Nothing reads the member
-- meanwhile.

DROP TRIGGER repo_watch_cursor_is_append_only ON repo_watch_cursor;

ALTER TABLE repo_watch_cursor
    DROP CONSTRAINT repo_watch_cursor_storage_version_check;

UPDATE repo_watch_cursor
   SET storage_version = 3,
       cursor_payload = jsonb_set(
           jsonb_set(cursor_payload, '{storage_version}', '3'::jsonb, false),
           '{event_identity_frontier}',
           (
               SELECT coalesce(
                          jsonb_agg(
                              carried.entry
                                  || '{"pull_request_number": null}'::jsonb
                              ORDER BY carried.position
                          ),
                          '[]'::jsonb
                      )
                 FROM jsonb_array_elements(
                          cursor_payload -> 'event_identity_frontier'
                      ) WITH ORDINALITY AS carried(entry, position)
           ),
           true
       );

ALTER TABLE repo_watch_cursor
    ADD CONSTRAINT repo_watch_cursor_storage_version_check
        CHECK (storage_version = 3);

CREATE TRIGGER repo_watch_cursor_is_append_only
BEFORE UPDATE OR DELETE ON repo_watch_cursor
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();
