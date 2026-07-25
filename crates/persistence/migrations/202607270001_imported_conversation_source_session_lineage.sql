-- Queryable source-session evidence groups exact imported snapshots without
-- participating in imported-conversation identity.

ALTER TABLE imported_conversation
    ADD COLUMN source_session_id bytea;

CREATE INDEX imported_conversation_source_session_id_idx
    ON imported_conversation USING hash (source_session_id)
    WHERE source_session_id IS NOT NULL;
