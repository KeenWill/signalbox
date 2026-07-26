-- Keep the constructor-selected metadata issuer independent from the actor
-- projection whose corruption checked reconstitution must detect.

ALTER TABLE replace_session_metadata_command
    ADD COLUMN issuer_kind text,
    ADD COLUMN issuer_tool_request_id uuid;

-- Metadata receipts are normally append-only. This migration owns the one-time
-- issuer backfill and restores the guard before sealing the new evidence.
ALTER TABLE replace_session_metadata_command
    DISABLE TRIGGER replace_session_metadata_command_is_append_only;

UPDATE replace_session_metadata_command
   SET issuer_kind = 'owner',
       issuer_tool_request_id = NULL;

ALTER TABLE replace_session_metadata_command
    ENABLE TRIGGER replace_session_metadata_command_is_append_only;

ALTER TABLE replace_session_metadata_command
    ALTER COLUMN issuer_kind SET NOT NULL,
    ADD CONSTRAINT replace_session_metadata_command_issuer_shape
        CHECK (
            (
                issuer_kind = 'owner'
                AND issuer_tool_request_id IS NULL
            )
            OR (
                issuer_kind = 'tool'
                AND issuer_tool_request_id IS NOT NULL
            )
        );
