-- Keep repository keyset pagination identical to the browser's canonical
-- code-point ordering regardless of the database's default collation.

CREATE INDEX repo_watch_repository_key_c_order
    ON repo_watch_repository_key ((repository COLLATE "C"));
