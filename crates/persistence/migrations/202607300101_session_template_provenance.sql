-- Immutable copy-on-create session-template provenance.
--
-- Static templates remain outside PostgreSQL. These columns retain only the
-- configured name and domain-derived SHA-256 digest of the bundle copied into
-- defaults version one. Existing and explicit sessions carry two nulls.

ALTER TABLE durable_command
    DROP CONSTRAINT durable_command_storage_version_supported,
    ADD CONSTRAINT durable_command_storage_version_supported
        CHECK (
            (
                command_kind = 'create_session'
                AND storage_version IN (1, 2, 3, 4)
            )
            OR (
                command_kind IN (
                    'create_session_from_imported_frontier',
                    'replace_session_defaults'
                )
                AND storage_version IN (1, 2, 3)
            )
            OR (
                command_kind IN (
                    'replace_session_metadata',
                    'submit_input',
                    'decide_tool_request',
                    'review_workflow'
                )
                AND storage_version = 1
            )
        );

ALTER TABLE create_session_command
    DROP CONSTRAINT create_session_command_storage_version_supported,
    ADD CONSTRAINT create_session_command_storage_version_supported
        CHECK (storage_version IN (1, 2, 3, 4));

ALTER TABLE session
    ADD COLUMN template_name text,
    ADD COLUMN template_content_digest bytea,
    ADD CONSTRAINT session_template_provenance_shape
        CHECK (
            (template_name IS NULL AND template_content_digest IS NULL)
            OR (
                template_name IS NOT NULL
                AND template_content_digest IS NOT NULL
                AND ancestry_kind = 'none'
                AND octet_length(convert_to(template_name, 'UTF8')) BETWEEN 1 AND 128
                AND template_name ~ '^[a-z0-9][a-z0-9._-]*$'
                AND octet_length(template_content_digest) = 32
            )
        ),
    ADD CONSTRAINT session_template_provenance_key
        UNIQUE (session_id, template_name, template_content_digest);

ALTER TABLE create_session_command
    ADD COLUMN template_name text,
    ADD COLUMN template_content_digest bytea,
    ADD CONSTRAINT create_session_command_template_provenance_shape
        CHECK (
            (template_name IS NULL AND template_content_digest IS NULL)
            OR (
                template_name IS NOT NULL
                AND template_content_digest IS NOT NULL
                AND octet_length(convert_to(template_name, 'UTF8')) BETWEEN 1 AND 128
                AND template_name ~ '^[a-z0-9][a-z0-9._-]*$'
                AND octet_length(template_content_digest) = 32
            )
        ),
    ADD CONSTRAINT create_session_command_template_provenance_versioned
        CHECK (
            storage_version >= 4
            OR (template_name IS NULL AND template_content_digest IS NULL)
        ),
    ADD CONSTRAINT create_session_command_template_provenance_fk
        FOREIGN KEY (
            created_session_id,
            template_name,
            template_content_digest
        )
        REFERENCES session (
            session_id,
            template_name,
            template_content_digest
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT;
