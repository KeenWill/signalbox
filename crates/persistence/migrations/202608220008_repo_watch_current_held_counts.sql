-- Keep repository status reads bounded as current held-work backlogs grow.

CREATE TABLE repo_watch_current_repository_held_count (
    repository text PRIMARY KEY,
    held_count bigint NOT NULL,

    CHECK (repo_watch_repository_is_valid(repository)),
    CHECK (held_count > 0)
);

INSERT INTO repo_watch_current_repository_held_count (repository, held_count)
SELECT repository, count(*)
  FROM repo_watch_current_held_dispatch
 GROUP BY repository;

CREATE FUNCTION maintain_repo_watch_repository_held_count()
RETURNS trigger
LANGUAGE plpgsql
SET search_path FROM CURRENT
AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        INSERT INTO repo_watch_current_repository_held_count (
            repository, held_count
        ) VALUES (
            NEW.repository, 1
        )
        ON CONFLICT (repository) DO UPDATE
            SET held_count =
                repo_watch_current_repository_held_count.held_count + 1;
        RETURN NULL;
    END IF;

    UPDATE repo_watch_current_repository_held_count
       SET held_count = held_count - 1
     WHERE repository = OLD.repository
       AND held_count > 1;
    IF FOUND THEN
        RETURN NULL;
    END IF;

    DELETE FROM repo_watch_current_repository_held_count
     WHERE repository = OLD.repository
       AND held_count = 1;
    IF NOT FOUND THEN
        RAISE EXCEPTION
            'current repository-watch held count is missing for %',
            OLD.repository;
    END IF;
    RETURN NULL;
END;
$$;

CREATE TRIGGER repo_watch_current_held_dispatch_maintains_repository_count
AFTER INSERT OR DELETE ON repo_watch_current_held_dispatch
FOR EACH ROW EXECUTE FUNCTION maintain_repo_watch_repository_held_count();
