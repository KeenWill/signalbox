-- One readable content-identity version for repository-watch events.
--
-- 202608150001_repo_watch_event_content_identity.sql introduced content
-- identity and, because the events already recorded predate it, marked those
-- rows version zero and admitted both versions. Version-tolerant decoding and
-- legacy-value aliases are defects under the pre-alpha rule, so this migration
-- completes the carry: the rows it marked version zero move to version one, and
-- exactly one version stays readable afterwards.
--
-- Dispatch rows reference repo_watch_event under ON DELETE RESTRICT, so the
-- carried rows are rewritten rather than discarded. Their identity is derived
-- under a hash domain reserved for this migration and disjoint from the
-- differ's, so a carried row can never claim an identity a producer would also
-- derive, and never matches one.

DROP TRIGGER repo_watch_event_is_append_only ON repo_watch_event;

UPDATE repo_watch_event
   SET content_identity_version = 1,
       content_identity = sha256(
           convert_to(
               'signalbox/repo-watch/migrated-event-identity/v1',
               'UTF8'
           ) || uuid_send(event_id)
       )
 WHERE content_identity_version = 0;

-- Supersedes the repo_watch_event_content_identity_version_check definition in
-- 202608150001_repo_watch_event_content_identity.sql, which admitted both
-- version zero and version one while the carry above was outstanding.
ALTER TABLE repo_watch_event
    DROP CONSTRAINT repo_watch_event_content_identity_version_check;

ALTER TABLE repo_watch_event
    ADD CONSTRAINT repo_watch_event_content_identity_version_check
        CHECK (content_identity_version = 1);

CREATE TRIGGER repo_watch_event_is_append_only
BEFORE UPDATE OR DELETE ON repo_watch_event
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();
