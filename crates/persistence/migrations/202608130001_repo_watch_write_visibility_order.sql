-- Serialize GitHub write receipts with repository-watch event recording so
-- their timestamps describe visibility order rather than transaction start.

CREATE FUNCTION lock_repo_watch_github_write_visibility()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    PERFORM pg_advisory_xact_lock(
        hashtextextended('repo-watch' || chr(31) || 'github-write-visibility', 0)
    );
    RETURN NULL;
END;
$$;

ALTER TABLE repo_watch_github_write_receipt
    ALTER COLUMN recorded_at SET DEFAULT clock_timestamp();

ALTER TABLE repo_watch_event
    ALTER COLUMN recorded_at SET DEFAULT clock_timestamp();

CREATE TRIGGER repo_watch_github_write_receipt_serializes_visibility
BEFORE INSERT ON repo_watch_github_write_receipt
FOR EACH STATEMENT
EXECUTE FUNCTION lock_repo_watch_github_write_visibility();

CREATE TRIGGER repo_watch_event_serializes_github_write_visibility
BEFORE INSERT ON repo_watch_event
FOR EACH STATEMENT
EXECUTE FUNCTION lock_repo_watch_github_write_visibility();
