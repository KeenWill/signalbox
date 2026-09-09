-- growth: one mutable row per session that binds daemon-local workspace tools.
-- release: deletion of the owning session.
CREATE TABLE session_workspace_binding (
    session_id uuid PRIMARY KEY REFERENCES session(session_id) ON DELETE CASCADE,
    workspace_root_kind text NOT NULL
        CHECK (workspace_root_kind IN ('derived', 'configured', 'provisioned'))
);
