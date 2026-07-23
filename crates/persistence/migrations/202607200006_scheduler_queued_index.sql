-- The periodic reconciliation in docs/spec/turn-lifecycle-and-scheduling.md
-- reads queued sessions for the lifetime of the database. Keep terminal
-- history out of that recurring access path.

CREATE INDEX turn_lifecycle_queued_by_session
    ON turn_lifecycle (session_id)
    WHERE state_kind = 'queued';
