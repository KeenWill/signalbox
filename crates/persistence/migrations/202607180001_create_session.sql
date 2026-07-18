-- Initial owner-initiated, no-ancestry CreateSession storage.
--
-- Domain identities are supplied by the application and therefore have no
-- database defaults. The generated model_selection_reference columns merely
-- expose one of two caller-supplied typed UUID fields for composite foreign-key
-- correlation; they do not mint an identity.

CREATE FUNCTION reject_immutable_record_change()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION '% is append-only', TG_TABLE_NAME
        USING ERRCODE = '23514';
END;
$$;

CREATE TABLE durable_command (
    command_id uuid PRIMARY KEY,
    command_kind text NOT NULL,
    storage_version smallint NOT NULL,
    claimed_at timestamptz NOT NULL,

    CONSTRAINT durable_command_kind_closed
        CHECK (command_kind = 'create_session'),
    CONSTRAINT durable_command_storage_version_supported
        CHECK (storage_version = 1),
    CONSTRAINT durable_command_kind_version_key
        UNIQUE (command_id, command_kind, storage_version)
);

CREATE TABLE session (
    session_id uuid PRIMARY KEY,
    creation_cause text NOT NULL,
    ancestry_kind text NOT NULL,

    CONSTRAINT session_creation_cause_closed
        CHECK (creation_cause = 'owner_initiated'),
    CONSTRAINT session_ancestry_kind_closed
        CHECK (ancestry_kind = 'none'),
    CONSTRAINT session_provenance_key
        UNIQUE (session_id, creation_cause, ancestry_kind)
);

CREATE TABLE session_defaults_version (
    session_id uuid NOT NULL,
    version numeric(20, 0) NOT NULL,
    model_selection_kind text NOT NULL,
    direct_model_selection_id uuid,
    model_alias_id uuid,
    model_selection_reference uuid GENERATED ALWAYS AS (
        COALESCE(direct_model_selection_id, model_alias_id)
    ) STORED,

    CONSTRAINT session_defaults_version_pk
        PRIMARY KEY (session_id, version),
    CONSTRAINT session_defaults_version_session_fk
        FOREIGN KEY (session_id)
        REFERENCES session (session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CONSTRAINT session_defaults_version_positive_u64
        CHECK (
            version >= 1
            AND version <= 18446744073709551615
        ),
    CONSTRAINT session_defaults_version_model_selection_kind_closed
        CHECK (model_selection_kind IN ('direct', 'alias')),
    CONSTRAINT session_defaults_version_model_selection_shape
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
    CONSTRAINT session_defaults_version_selection_key
        UNIQUE (
            session_id,
            version,
            model_selection_kind,
            model_selection_reference
        )
);

CREATE TABLE session_current_defaults (
    session_id uuid PRIMARY KEY,
    current_version numeric(20, 0) NOT NULL,

    CONSTRAINT session_current_defaults_version_positive_u64
        CHECK (
            current_version >= 1
            AND current_version <= 18446744073709551615
        ),
    CONSTRAINT session_current_defaults_version_fk
        FOREIGN KEY (session_id, current_version)
        REFERENCES session_defaults_version (session_id, version)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE TABLE create_session_command (
    command_id uuid PRIMARY KEY,
    command_kind text NOT NULL,
    storage_version smallint NOT NULL,
    creation_cause text NOT NULL,
    ancestry_kind text NOT NULL,
    initial_defaults_version numeric(20, 0) NOT NULL,
    model_selection_kind text NOT NULL,
    direct_model_selection_id uuid,
    model_alias_id uuid,
    model_selection_reference uuid GENERATED ALWAYS AS (
        COALESCE(direct_model_selection_id, model_alias_id)
    ) STORED,
    result_kind text NOT NULL,
    created_session_id uuid NOT NULL UNIQUE,

    CONSTRAINT create_session_command_kind_closed
        CHECK (command_kind = 'create_session'),
    CONSTRAINT create_session_command_storage_version_supported
        CHECK (storage_version = 1),
    CONSTRAINT create_session_command_creation_cause_closed
        CHECK (creation_cause = 'owner_initiated'),
    CONSTRAINT create_session_command_ancestry_kind_closed
        CHECK (ancestry_kind = 'none'),
    CONSTRAINT create_session_command_initial_defaults_version
        CHECK (initial_defaults_version = 1),
    CONSTRAINT create_session_command_model_selection_kind_closed
        CHECK (model_selection_kind IN ('direct', 'alias')),
    CONSTRAINT create_session_command_model_selection_shape
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
    CONSTRAINT create_session_command_result_kind_closed
        CHECK (result_kind = 'applied'),
    CONSTRAINT create_session_command_registry_fk
        FOREIGN KEY (command_id, command_kind, storage_version)
        REFERENCES durable_command (
            command_id,
            command_kind,
            storage_version
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT create_session_command_provenance_fk
        FOREIGN KEY (created_session_id, creation_cause, ancestry_kind)
        REFERENCES session (session_id, creation_cause, ancestry_kind)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CONSTRAINT create_session_command_initial_defaults_fk
        FOREIGN KEY (
            created_session_id,
            initial_defaults_version,
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
);

-- These deferred reverse references require one complete typed command record
-- per claimed registry ID, one current-defaults pointer per session, and one
-- backing CreateSession record per session at every transaction boundary, while
-- still allowing the rows to be inserted together.
ALTER TABLE durable_command
    ADD CONSTRAINT durable_command_typed_record_fk
    FOREIGN KEY (command_id)
    REFERENCES create_session_command (command_id)
    ON UPDATE RESTRICT
    ON DELETE RESTRICT
    DEFERRABLE INITIALLY DEFERRED;

ALTER TABLE session
    ADD CONSTRAINT session_current_defaults_fk
    FOREIGN KEY (session_id)
    REFERENCES session_current_defaults (session_id)
    ON UPDATE RESTRICT
    ON DELETE RESTRICT
    DEFERRABLE INITIALLY DEFERRED;

ALTER TABLE session
    ADD CONSTRAINT session_create_command_fk
    FOREIGN KEY (session_id)
    REFERENCES create_session_command (created_session_id)
    ON UPDATE RESTRICT
    ON DELETE RESTRICT
    DEFERRABLE INITIALLY DEFERRED;

CREATE TRIGGER durable_command_is_append_only
BEFORE UPDATE OR DELETE ON durable_command
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER session_is_append_only
BEFORE UPDATE OR DELETE ON session
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER session_defaults_version_is_append_only
BEFORE UPDATE OR DELETE ON session_defaults_version
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER create_session_command_is_append_only
BEFORE UPDATE OR DELETE ON create_session_command
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();
