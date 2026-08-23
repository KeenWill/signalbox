-- Keep repository status reads bounded as outstanding obligation backlogs grow.

CREATE TABLE repo_watch_current_repository_obligation_count (
    repository text PRIMARY KEY,
    obligation_count bigint NOT NULL,

    CHECK (repo_watch_repository_is_valid(repository)),
    CHECK (obligation_count > 0)
);

INSERT INTO repo_watch_current_repository_obligation_count (
    repository, obligation_count
)
SELECT repository, count(*)
  FROM repo_watch_dispatch_obligation
 WHERE settled_kind IS NULL
 GROUP BY repository;

CREATE FUNCTION increment_repo_watch_repository_obligation_count(
    counted_repository text
)
RETURNS void
LANGUAGE plpgsql
SET search_path FROM CURRENT
AS $$
BEGIN
    INSERT INTO repo_watch_current_repository_obligation_count (
        repository, obligation_count
    ) VALUES (
        counted_repository, 1
    )
    ON CONFLICT (repository) DO UPDATE
        SET obligation_count =
            repo_watch_current_repository_obligation_count.obligation_count + 1;
END;
$$;

CREATE FUNCTION decrement_repo_watch_repository_obligation_count(
    counted_repository text
)
RETURNS void
LANGUAGE plpgsql
SET search_path FROM CURRENT
AS $$
BEGIN
    UPDATE repo_watch_current_repository_obligation_count
       SET obligation_count = obligation_count - 1
     WHERE repository = counted_repository
       AND obligation_count > 1;
    IF FOUND THEN
        RETURN;
    END IF;

    DELETE FROM repo_watch_current_repository_obligation_count
     WHERE repository = counted_repository
       AND obligation_count = 1;
    IF NOT FOUND THEN
        RAISE EXCEPTION
            'outstanding repository-watch obligation count is missing for %',
            counted_repository;
    END IF;
END;
$$;

CREATE FUNCTION maintain_repo_watch_repository_obligation_count()
RETURNS trigger
LANGUAGE plpgsql
SET search_path FROM CURRENT
AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        IF NEW.settled_kind IS NULL THEN
            PERFORM increment_repo_watch_repository_obligation_count(NEW.repository);
        END IF;
        RETURN NULL;
    END IF;

    IF OLD.settled_kind IS NULL
       AND (NEW.settled_kind IS NOT NULL OR OLD.repository <> NEW.repository) THEN
        PERFORM decrement_repo_watch_repository_obligation_count(OLD.repository);
    END IF;
    IF NEW.settled_kind IS NULL
       AND (OLD.settled_kind IS NOT NULL OR OLD.repository <> NEW.repository) THEN
        PERFORM increment_repo_watch_repository_obligation_count(NEW.repository);
    END IF;
    RETURN NULL;
END;
$$;

CREATE TRIGGER repo_watch_dispatch_obligation_maintains_repository_count
AFTER INSERT OR UPDATE OF repository, settled_kind
ON repo_watch_dispatch_obligation
FOR EACH ROW EXECUTE FUNCTION maintain_repo_watch_repository_obligation_count();
