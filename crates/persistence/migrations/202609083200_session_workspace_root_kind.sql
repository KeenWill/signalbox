-- Session-owned binding evidence; retained with its session and containing no path.
ALTER TABLE session ADD COLUMN workspace_root_kind text
    CHECK (workspace_root_kind IN ('derived', 'configured', 'provisioned'));
