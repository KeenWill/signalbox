-- Typed persistence for session-defaults replacement
-- (docs/spec/sessions-and-transcript.md).
--
-- The owner-global registry remains the single claim boundary. Its former
-- reverse foreign key could name only CreateSession, so this migration replaces
-- that one-kind shape with a closed deferred constraint trigger.

ALTER TABLE durable_command
    DROP CONSTRAINT durable_command_kind_closed;

ALTER TABLE durable_command
    ADD CONSTRAINT durable_command_kind_closed
    CHECK (
        command_kind IN (
            'create_session',
            'replace_session_defaults'
        )
    );

CREATE TABLE replace_session_defaults_command (
    command_id uuid PRIMARY KEY,
    command_kind text NOT NULL,
    storage_version smallint NOT NULL,
    session_id uuid NOT NULL,
    expected_current_version numeric(20, 0) NOT NULL,
    model_selection_kind text NOT NULL,
    direct_model_selection_id uuid,
    model_alias_id uuid,
    model_selection_reference uuid GENERATED ALWAYS AS (
        COALESCE(direct_model_selection_id, model_alias_id)
    ) STORED,
    result_kind text NOT NULL,
    rejection_kind text,
    result_session_id uuid NOT NULL,
    result_installed_version numeric(20, 0),
    result_expected_version numeric(20, 0),
    result_current_version numeric(20, 0),

    CONSTRAINT replace_session_defaults_command_kind_closed
        CHECK (command_kind = 'replace_session_defaults'),
    CONSTRAINT replace_session_defaults_command_storage_version_supported
        CHECK (storage_version = 1),
    CONSTRAINT replace_session_defaults_command_expected_version_positive_u64
        CHECK (
            expected_current_version >= 1
            AND expected_current_version <= 18446744073709551615
        ),
    CONSTRAINT replace_session_defaults_command_model_selection_kind_closed
        CHECK (model_selection_kind IN ('direct', 'alias')),
    CONSTRAINT replace_session_defaults_command_model_selection_shape
        CHECK (
            (
                model_selection_kind = 'direct'
                AND direct_model_selection_id IS NOT NULL
                AND model_alias_id IS NULL
            )
            OR
            (
                model_selection_kind = 'alias'
                AND direct_model_selection_id IS NULL
                AND model_alias_id IS NOT NULL
            )
        ),
    CONSTRAINT replace_session_defaults_command_result_kind_closed
        CHECK (result_kind IN ('applied', 'rejected')),
    CONSTRAINT replace_session_defaults_command_rejection_kind_closed
        CHECK (
            rejection_kind IS NULL
            OR rejection_kind IN (
                'session_not_found',
                'current_version_mismatch',
                'version_exhausted'
            )
        ),
    CONSTRAINT replace_session_defaults_command_result_session_matches
        CHECK (result_session_id = session_id),
    CONSTRAINT replace_session_defaults_command_installed_version_positive_u64
        CHECK (
            result_installed_version IS NULL
            OR (
                result_installed_version >= 1
                AND result_installed_version <= 18446744073709551615
            )
        ),
    CONSTRAINT replace_session_defaults_command_result_expected_positive_u64
        CHECK (
            result_expected_version IS NULL
            OR (
                result_expected_version >= 1
                AND result_expected_version <= 18446744073709551615
            )
        ),
    CONSTRAINT replace_session_defaults_command_result_current_positive_u64
        CHECK (
            result_current_version IS NULL
            OR (
                result_current_version >= 1
                AND result_current_version <= 18446744073709551615
            )
        ),
    CONSTRAINT replace_session_defaults_command_result_shape
        CHECK (
            (
                result_kind = 'applied'
                AND rejection_kind IS NULL
                AND result_installed_version IS NOT NULL
                AND result_expected_version IS NULL
                AND result_current_version IS NULL
                AND result_installed_version = expected_current_version + 1
            )
            OR
            (
                result_kind = 'rejected'
                AND rejection_kind = 'session_not_found'
                AND result_installed_version IS NULL
                AND result_expected_version IS NULL
                AND result_current_version IS NULL
            )
            OR
            (
                result_kind = 'rejected'
                AND rejection_kind = 'current_version_mismatch'
                AND result_installed_version IS NULL
                AND result_expected_version = expected_current_version
                AND result_current_version IS NOT NULL
                AND result_current_version <> result_expected_version
            )
            OR
            (
                result_kind = 'rejected'
                AND rejection_kind = 'version_exhausted'
                AND result_installed_version IS NULL
                AND result_expected_version IS NULL
                AND result_current_version = expected_current_version
                AND result_current_version = 18446744073709551615
            )
        ),
    CONSTRAINT replace_session_defaults_command_registry_fk
        FOREIGN KEY (command_id, command_kind, storage_version)
        REFERENCES durable_command (
            command_id,
            command_kind,
            storage_version
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT replace_session_defaults_command_applied_defaults_fk
        FOREIGN KEY (
            result_session_id,
            result_installed_version,
            model_selection_kind,
            model_selection_reference
        )
        REFERENCES session_defaults_version (
            session_id,
            version,
            model_selection_kind,
            model_selection_reference
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE TRIGGER replace_session_defaults_command_is_append_only
BEFORE UPDATE OR DELETE ON replace_session_defaults_command
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

ALTER TABLE durable_command
    DROP CONSTRAINT durable_command_typed_record_fk;

CREATE FUNCTION require_durable_command_typed_record()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    matching_records bigint;
BEGIN
    CASE NEW.command_kind
        WHEN 'create_session' THEN
            SELECT count(*)
              INTO matching_records
              FROM create_session_command
             WHERE command_id = NEW.command_id;
        WHEN 'replace_session_defaults' THEN
            SELECT count(*)
              INTO matching_records
              FROM replace_session_defaults_command
             WHERE command_id = NEW.command_id;
        ELSE
            RAISE EXCEPTION 'unsupported durable command kind %', NEW.command_kind
                USING ERRCODE = '23514';
    END CASE;

    IF matching_records <> 1 THEN
        RAISE EXCEPTION
            'durable command % requires exactly one % typed record',
            NEW.command_id,
            NEW.command_kind
            USING ERRCODE = '23503';
    END IF;

    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER durable_command_requires_typed_record
AFTER INSERT ON durable_command
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_durable_command_typed_record();

-- The dropped reverse foreign key established this property for every
-- pre-migration CreateSession row. Keep migration-time validation explicit so
-- a future change cannot make that assumption invisible.
DO $$
BEGIN
    IF EXISTS (
        SELECT 1
          FROM durable_command AS command
          LEFT JOIN create_session_command AS typed
            ON typed.command_id = command.command_id
         WHERE command.command_kind = 'create_session'
           AND typed.command_id IS NULL
    ) THEN
        RAISE EXCEPTION 'preexisting durable command lacks typed record'
            USING ERRCODE = '23503';
    END IF;
END;
$$;
