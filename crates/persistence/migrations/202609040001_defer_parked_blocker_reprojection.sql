-- Blocker replacement while an obligation is parked remains deferred.

DROP TRIGGER repo_watch_obligation_parks_core_session
    ON repo_watch_dispatch_obligation;

CREATE CONSTRAINT TRIGGER repo_watch_obligation_parks_core_session
    AFTER UPDATE OF parked_at, settled_kind
    ON repo_watch_dispatch_obligation
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW
    WHEN (OLD.parked_at IS DISTINCT FROM NEW.parked_at
          OR OLD.settled_kind IS DISTINCT FROM NEW.settled_kind)
    EXECUTE FUNCTION park_repo_watch_obligation_sessions();
