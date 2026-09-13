ALTER TABLE replace_session_metadata_command
    ADD COLUMN title_only boolean NOT NULL DEFAULT false,
    ADD CONSTRAINT replace_session_metadata_command_title_request_shape
        CHECK (NOT title_only OR (actor_kind = 'user' AND replacement_title IS NOT NULL));
