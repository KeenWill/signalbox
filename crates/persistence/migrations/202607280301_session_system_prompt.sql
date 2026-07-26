-- One optional bounded session system prompt inside the immutable defaults
-- epoch and the commands that install it.
--
-- The decision log entry dated 2026-07-26 records the owner's one-mebibyte
-- (1,048,576 UTF-8 bytes) bound on the optional session system prompt,
-- mirroring the accepted-input content bound. The prompt lives only in the
-- immutable defaults version and its installing command records; per-turn
-- origin rows keep binding the epoch by frozen version, so no queued-input
-- column is added. Existing rows predate the column and remain promptless.

ALTER TABLE durable_command
    DROP CONSTRAINT durable_command_storage_version_supported,
    ADD CONSTRAINT durable_command_storage_version_supported
        CHECK (
            (
                command_kind IN (
                    'create_session',
                    'create_session_from_imported_frontier',
                    'replace_session_defaults'
                )
                AND storage_version IN (1, 2, 3)
            )
            OR (
                command_kind IN (
                    'replace_session_metadata',
                    'submit_input',
                    'decide_tool_request'
                )
                AND storage_version = 1
            )
        );

ALTER TABLE create_session_command
    DROP CONSTRAINT create_session_command_storage_version_supported,
    ADD CONSTRAINT create_session_command_storage_version_supported
        CHECK (storage_version IN (1, 2, 3));

ALTER TABLE replace_session_defaults_command
    DROP CONSTRAINT replace_session_defaults_command_storage_version_supported,
    ADD CONSTRAINT replace_session_defaults_command_storage_version_supported
        CHECK (storage_version IN (1, 2, 3));

ALTER TABLE create_session_from_imported_frontier_command
    DROP CONSTRAINT
        create_session_from_imported_frontier_command_version_supported,
    ADD CONSTRAINT
        create_session_from_imported_frontier_command_version_supported
        CHECK (storage_version IN (1, 2, 3));

-- The command/defaults agreement foreign keys below need the prompt inside
-- the referenced unique key, but a PostgreSQL btree index entry cannot hold
-- megabyte text. A 32-byte SHA-256 digest of the exact UTF-8 encoding stands
-- in for the text; the empty bytea marks an absent prompt so the MATCH SIMPLE
-- foreign keys never skip enforcement through a NULL member. convert_to is
-- only stable in general because it reads the session's server encoding, so
-- this deployment-pinned UTF-8 database wraps it in an immutable function for
-- the generated columns.
CREATE FUNCTION session_system_prompt_digest(prompt text) RETURNS bytea
    LANGUAGE sql
    IMMUTABLE
    PARALLEL SAFE
    RETURN COALESCE(sha256(convert_to(prompt, 'UTF8')), '\x'::bytea);

ALTER TABLE session_defaults_version
    ADD COLUMN system_prompt text,
    ADD COLUMN system_prompt_digest bytea GENERATED ALWAYS AS (
        session_system_prompt_digest(system_prompt)
    ) STORED NOT NULL,
    ADD CONSTRAINT session_defaults_version_system_prompt_bounded
        CHECK (
            system_prompt IS NULL
            OR (
                octet_length(convert_to(system_prompt, 'UTF8')) >= 1
                AND octet_length(convert_to(system_prompt, 'UTF8')) <= 1048576
            )
        );

ALTER TABLE create_session_command
    ADD COLUMN system_prompt text,
    ADD COLUMN system_prompt_digest bytea GENERATED ALWAYS AS (
        session_system_prompt_digest(system_prompt)
    ) STORED NOT NULL,
    ADD CONSTRAINT create_session_command_system_prompt_bounded
        CHECK (
            system_prompt IS NULL
            OR (
                octet_length(convert_to(system_prompt, 'UTF8')) >= 1
                AND octet_length(convert_to(system_prompt, 'UTF8')) <= 1048576
            )
        ),
    ADD CONSTRAINT create_session_command_system_prompt_versioned
        CHECK (system_prompt IS NULL OR storage_version >= 3);

ALTER TABLE replace_session_defaults_command
    ADD COLUMN system_prompt text,
    ADD COLUMN system_prompt_digest bytea GENERATED ALWAYS AS (
        session_system_prompt_digest(system_prompt)
    ) STORED NOT NULL,
    ADD CONSTRAINT replace_session_defaults_command_system_prompt_bounded
        CHECK (
            system_prompt IS NULL
            OR (
                octet_length(convert_to(system_prompt, 'UTF8')) >= 1
                AND octet_length(convert_to(system_prompt, 'UTF8')) <= 1048576
            )
        ),
    ADD CONSTRAINT replace_session_defaults_command_system_prompt_versioned
        CHECK (system_prompt IS NULL OR storage_version >= 3);

ALTER TABLE create_session_from_imported_frontier_command
    ADD COLUMN system_prompt text,
    ADD COLUMN system_prompt_digest bytea GENERATED ALWAYS AS (
        session_system_prompt_digest(system_prompt)
    ) STORED NOT NULL,
    ADD CONSTRAINT imported_frontier_command_system_prompt_bounded
        CHECK (
            system_prompt IS NULL
            OR (
                octet_length(convert_to(system_prompt, 'UTF8')) >= 1
                AND octet_length(convert_to(system_prompt, 'UTF8')) <= 1048576
            )
        ),
    ADD CONSTRAINT imported_frontier_command_system_prompt_versioned
        CHECK (system_prompt IS NULL OR storage_version >= 3);

-- Rebuild the selection key and the three command/defaults agreement foreign
-- keys so an installing command's recorded prompt must agree byte for byte
-- (through its exact-encoding digest) with the installed defaults version.

ALTER TABLE create_session_command
    DROP CONSTRAINT create_session_command_initial_defaults_fk;

ALTER TABLE replace_session_defaults_command
    DROP CONSTRAINT replace_session_defaults_command_applied_defaults_fk;

ALTER TABLE create_session_from_imported_frontier_command
    DROP CONSTRAINT create_session_from_imported_frontier_command_defaults_fk;

ALTER TABLE session_defaults_version
    DROP CONSTRAINT session_defaults_version_selection_key,
    ADD CONSTRAINT session_defaults_version_selection_key
        UNIQUE (
            session_id,
            version,
            model_selection_kind,
            model_selection_reference,
            dangerous_tool_auto_approval,
            system_prompt_digest
        );

ALTER TABLE create_session_command
    ADD CONSTRAINT create_session_command_initial_defaults_fk
        FOREIGN KEY (
            created_session_id,
            initial_defaults_version,
            model_selection_kind,
            model_selection_reference,
            dangerous_tool_auto_approval,
            system_prompt_digest
        )
        REFERENCES session_defaults_version (
            session_id,
            version,
            model_selection_kind,
            model_selection_reference,
            dangerous_tool_auto_approval,
            system_prompt_digest
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT;

ALTER TABLE replace_session_defaults_command
    ADD CONSTRAINT replace_session_defaults_command_applied_defaults_fk
        FOREIGN KEY (
            result_session_id,
            result_installed_version,
            model_selection_kind,
            model_selection_reference,
            dangerous_tool_auto_approval,
            system_prompt_digest
        )
        REFERENCES session_defaults_version (
            session_id,
            version,
            model_selection_kind,
            model_selection_reference,
            dangerous_tool_auto_approval,
            system_prompt_digest
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED;

ALTER TABLE create_session_from_imported_frontier_command
    ADD CONSTRAINT create_session_from_imported_frontier_command_defaults_fk
        FOREIGN KEY (
            created_session_id,
            initial_defaults_version,
            model_selection_kind,
            model_selection_reference,
            dangerous_tool_auto_approval,
            system_prompt_digest
        )
        REFERENCES session_defaults_version (
            session_id,
            version,
            model_selection_kind,
            model_selection_reference,
            dangerous_tool_auto_approval,
            system_prompt_digest
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED;
