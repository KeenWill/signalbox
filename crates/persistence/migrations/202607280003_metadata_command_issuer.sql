-- Keep the constructor-selected metadata issuer independent from the actor
-- projection whose corruption checked reconstitution must detect.

ALTER TABLE replace_session_metadata_command
    ADD COLUMN issuer_kind text,
    ADD COLUMN issuer_tool_request_id uuid;

UPDATE replace_session_metadata_command
   SET issuer_kind = actor_kind,
       issuer_tool_request_id = actor_tool_request_id;

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
