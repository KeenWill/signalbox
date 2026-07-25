-- Replaceable session-organization satellites and their durable command.
--
-- Current metadata is mutable, normalized, and absent until the first write.
-- Durable command rows retain the complete caller payload and terminal receipt
-- independently of later last-writer-wins replacements.

ALTER TABLE durable_command
    DROP CONSTRAINT durable_command_kind_closed,
    DROP CONSTRAINT durable_command_storage_version_supported;

ALTER TABLE durable_command
    ADD CONSTRAINT durable_command_kind_closed
        CHECK (
            command_kind IN (
                'create_session',
                'create_session_from_imported_frontier',
                'replace_session_defaults',
                'replace_session_metadata',
                'submit_input',
                'decide_tool_request'
            )
        ),
    ADD CONSTRAINT durable_command_storage_version_supported
        CHECK (
            (
                command_kind IN (
                    'create_session',
                    'create_session_from_imported_frontier',
                    'replace_session_defaults'
                )
                AND storage_version IN (1, 2)
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

CREATE TABLE session_metadata (
    session_id uuid PRIMARY KEY,
    source_command_id uuid NOT NULL,
    title text,
    archived boolean NOT NULL,
    updated_at timestamptz NOT NULL,
    actor_kind text NOT NULL,
    actor_turn_id uuid,
    actor_tool_request_id uuid,

    CONSTRAINT session_metadata_session_fk
        FOREIGN KEY (session_id)
        REFERENCES session (session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CONSTRAINT session_metadata_title_nonempty
        CHECK (title IS NULL OR octet_length(title) > 0),
    CONSTRAINT session_metadata_updated_at_finite
        CHECK (
            updated_at > '-infinity'::timestamptz
            AND updated_at < 'infinity'::timestamptz
        ),
    CONSTRAINT session_metadata_actor_kind_closed
        CHECK (actor_kind IN ('owner', 'model', 'recovery', 'tool')),
    CONSTRAINT session_metadata_actor_shape
        CHECK (
            (
                actor_kind IN ('owner', 'recovery')
                AND actor_turn_id IS NULL
                AND actor_tool_request_id IS NULL
            )
            OR (
                actor_kind = 'model'
                AND actor_turn_id IS NOT NULL
                AND actor_tool_request_id IS NULL
            )
            OR (
                actor_kind = 'tool'
                AND actor_turn_id IS NULL
                AND actor_tool_request_id IS NOT NULL
            )
        )
);

CREATE TRIGGER session_metadata_delete_is_rejected
BEFORE DELETE ON session_metadata
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE FUNCTION reject_session_metadata_identity_change()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.session_id IS DISTINCT FROM OLD.session_id THEN
        RAISE EXCEPTION
            'session metadata identity is immutable'
            USING ERRCODE = '23514';
    END IF;

    RETURN NEW;
END;
$$;

CREATE TRIGGER session_metadata_identity_is_immutable
BEFORE UPDATE ON session_metadata
FOR EACH ROW
EXECUTE FUNCTION reject_session_metadata_identity_change();

CREATE TABLE session_metadata_tag (
    session_id uuid NOT NULL,
    tag text NOT NULL,

    CONSTRAINT session_metadata_tag_pk PRIMARY KEY (session_id, tag),
    CONSTRAINT session_metadata_tag_session_fk
        FOREIGN KEY (session_id)
        REFERENCES session_metadata (session_id)
        ON UPDATE RESTRICT
        ON DELETE CASCADE,
    CONSTRAINT session_metadata_tag_nonempty CHECK (octet_length(tag) > 0),
    CONSTRAINT session_metadata_tag_indexed_bound
        CHECK (octet_length(convert_to(tag, 'UTF8')) <= 1024)
);

CREATE TABLE session_metadata_attribute (
    session_id uuid NOT NULL,
    attribute_key text NOT NULL,
    attribute_value text NOT NULL,

    CONSTRAINT session_metadata_attribute_pk
        PRIMARY KEY (session_id, attribute_key),
    CONSTRAINT session_metadata_attribute_session_fk
        FOREIGN KEY (session_id)
        REFERENCES session_metadata (session_id)
        ON UPDATE RESTRICT
        ON DELETE CASCADE,
    CONSTRAINT session_metadata_attribute_key_nonempty
        CHECK (octet_length(attribute_key) > 0),
    CONSTRAINT session_metadata_attribute_key_indexed_bound
        CHECK (octet_length(convert_to(attribute_key, 'UTF8')) <= 1024)
);

CREATE INDEX session_metadata_tag_lookup
    ON session_metadata_tag (tag, session_id);

CREATE TABLE replace_session_metadata_command (
    command_id uuid PRIMARY KEY,
    command_kind text NOT NULL,
    storage_version smallint NOT NULL,
    session_id uuid NOT NULL,
    actor_kind text NOT NULL,
    actor_turn_id uuid,
    actor_tool_request_id uuid,
    replacement_title text,
    replacement_archived boolean NOT NULL,
    result_kind text NOT NULL,
    rejection_kind text,
    result_session_id uuid NOT NULL,
    result_applied_session_id uuid,
    result_updated_at timestamptz,
    result_actor_kind text,
    result_actor_turn_id uuid,
    result_actor_tool_request_id uuid,

    CONSTRAINT replace_session_metadata_command_kind_closed
        CHECK (command_kind = 'replace_session_metadata'),
    CONSTRAINT replace_session_metadata_command_storage_version_supported
        CHECK (storage_version = 1),
    CONSTRAINT replace_session_metadata_command_title_nonempty
        CHECK (
            replacement_title IS NULL
            OR octet_length(replacement_title) > 0
        ),
    CONSTRAINT replace_session_metadata_command_actor_kind_closed
        CHECK (actor_kind IN ('owner', 'model', 'recovery', 'tool')),
    CONSTRAINT replace_session_metadata_command_actor_shape
        CHECK (
            (
                actor_kind IN ('owner', 'recovery')
                AND actor_turn_id IS NULL
                AND actor_tool_request_id IS NULL
            )
            OR (
                actor_kind = 'model'
                AND actor_turn_id IS NOT NULL
                AND actor_tool_request_id IS NULL
            )
            OR (
                actor_kind = 'tool'
                AND actor_turn_id IS NULL
                AND actor_tool_request_id IS NOT NULL
            )
        ),
    CONSTRAINT replace_session_metadata_command_result_kind_closed
        CHECK (result_kind IN ('applied', 'rejected')),
    CONSTRAINT replace_session_metadata_command_rejection_kind_closed
        CHECK (
            rejection_kind IS NULL
            OR rejection_kind = 'session_not_found'
        ),
    CONSTRAINT replace_session_metadata_command_result_session_matches
        CHECK (result_session_id = session_id),
    CONSTRAINT replace_session_metadata_command_result_updated_at_finite
        CHECK (
            result_updated_at IS NULL
            OR (
                result_updated_at > '-infinity'::timestamptz
                AND result_updated_at < 'infinity'::timestamptz
            )
        ),
    CONSTRAINT replace_session_metadata_command_result_actor_kind_closed
        CHECK (
            result_actor_kind IS NULL
            OR result_actor_kind IN ('owner', 'model', 'recovery', 'tool')
        ),
    CONSTRAINT replace_session_metadata_command_result_actor_shape
        CHECK (
            (
                result_actor_kind IS NULL
                AND result_actor_turn_id IS NULL
                AND result_actor_tool_request_id IS NULL
            )
            OR (
                result_actor_kind IN ('owner', 'recovery')
                AND result_actor_turn_id IS NULL
                AND result_actor_tool_request_id IS NULL
            )
            OR (
                result_actor_kind = 'model'
                AND result_actor_turn_id IS NOT NULL
                AND result_actor_tool_request_id IS NULL
            )
            OR (
                result_actor_kind = 'tool'
                AND result_actor_turn_id IS NULL
                AND result_actor_tool_request_id IS NOT NULL
            )
        ),
    CONSTRAINT replace_session_metadata_command_result_shape
        CHECK (
            (
                result_kind = 'applied'
                AND rejection_kind IS NULL
                AND result_applied_session_id IS NOT NULL
                AND result_applied_session_id = session_id
                AND result_updated_at IS NOT NULL
                AND result_actor_kind IS NOT NULL
                AND result_actor_kind = actor_kind
                AND result_actor_turn_id IS NOT DISTINCT FROM actor_turn_id
                AND result_actor_tool_request_id
                    IS NOT DISTINCT FROM actor_tool_request_id
            )
            OR (
                result_kind = 'rejected'
                AND rejection_kind = 'session_not_found'
                AND result_applied_session_id IS NULL
                AND result_updated_at IS NULL
                AND result_actor_kind IS NULL
                AND result_actor_turn_id IS NULL
                AND result_actor_tool_request_id IS NULL
            )
        ),
    CONSTRAINT replace_session_metadata_command_registry_fk
        FOREIGN KEY (command_id, command_kind, storage_version)
        REFERENCES durable_command (
            command_id,
            command_kind,
            storage_version
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT replace_session_metadata_command_applied_session_fk
        FOREIGN KEY (result_applied_session_id)
        REFERENCES session_metadata (session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT replace_session_metadata_command_applied_session_unique
        UNIQUE (command_id, result_applied_session_id)
);

ALTER TABLE session_metadata
    ADD CONSTRAINT session_metadata_source_command_fk
        FOREIGN KEY (source_command_id, session_id)
        REFERENCES replace_session_metadata_command (
            command_id,
            result_applied_session_id
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED;

CREATE TABLE replace_session_metadata_command_tag (
    command_id uuid NOT NULL,
    tag text NOT NULL,

    CONSTRAINT replace_session_metadata_command_tag_pk
        PRIMARY KEY (command_id, tag),
    CONSTRAINT replace_session_metadata_command_tag_command_fk
        FOREIGN KEY (command_id)
        REFERENCES replace_session_metadata_command (command_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT replace_session_metadata_command_tag_nonempty
        CHECK (octet_length(tag) > 0),
    CONSTRAINT replace_session_metadata_command_tag_indexed_bound
        CHECK (octet_length(convert_to(tag, 'UTF8')) <= 1024)
);

CREATE TABLE replace_session_metadata_command_attribute (
    command_id uuid NOT NULL,
    attribute_key text NOT NULL,
    attribute_value text NOT NULL,

    CONSTRAINT replace_session_metadata_command_attribute_pk
        PRIMARY KEY (command_id, attribute_key),
    CONSTRAINT replace_session_metadata_command_attribute_command_fk
        FOREIGN KEY (command_id)
        REFERENCES replace_session_metadata_command (command_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT replace_session_metadata_command_attribute_key_nonempty
        CHECK (octet_length(attribute_key) > 0),
    CONSTRAINT replace_session_metadata_command_attribute_key_indexed_bound
        CHECK (octet_length(convert_to(attribute_key, 'UTF8')) <= 1024)
);

CREATE TRIGGER session_metadata_tag_update_is_rejected
BEFORE UPDATE ON session_metadata_tag
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER session_metadata_attribute_update_is_rejected
BEFORE UPDATE ON session_metadata_attribute
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE FUNCTION require_session_metadata_matches_receipt()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    checked_session_id uuid;
BEGIN
    IF TG_OP = 'DELETE' THEN
        checked_session_id := OLD.session_id;
    ELSE
        checked_session_id := NEW.session_id;
    END IF;

    IF EXISTS (
        SELECT 1
          FROM session_metadata AS current
          LEFT JOIN replace_session_metadata_command AS receipt
            ON receipt.command_id = current.source_command_id
           AND receipt.result_applied_session_id = current.session_id
         WHERE current.session_id = checked_session_id
           AND (
                receipt.command_id IS NULL
                OR receipt.result_kind <> 'applied'
                OR current.title IS DISTINCT FROM receipt.replacement_title
                OR current.archived IS DISTINCT FROM receipt.replacement_archived
                OR current.updated_at IS DISTINCT FROM receipt.result_updated_at
                OR current.actor_kind IS DISTINCT FROM receipt.result_actor_kind
                OR current.actor_turn_id
                    IS DISTINCT FROM receipt.result_actor_turn_id
                OR current.actor_tool_request_id
                    IS DISTINCT FROM receipt.result_actor_tool_request_id
                OR EXISTS (
                    SELECT current_tag.tag
                      FROM session_metadata_tag AS current_tag
                     WHERE current_tag.session_id = current.session_id
                    EXCEPT
                    SELECT receipt_tag.tag
                      FROM replace_session_metadata_command_tag AS receipt_tag
                     WHERE receipt_tag.command_id = receipt.command_id
                )
                OR EXISTS (
                    SELECT receipt_tag.tag
                      FROM replace_session_metadata_command_tag AS receipt_tag
                     WHERE receipt_tag.command_id = receipt.command_id
                    EXCEPT
                    SELECT current_tag.tag
                      FROM session_metadata_tag AS current_tag
                     WHERE current_tag.session_id = current.session_id
                )
                OR EXISTS (
                    SELECT
                        current_attribute.attribute_key,
                        current_attribute.attribute_value
                      FROM session_metadata_attribute AS current_attribute
                     WHERE current_attribute.session_id = current.session_id
                    EXCEPT
                    SELECT
                        receipt_attribute.attribute_key,
                        receipt_attribute.attribute_value
                      FROM replace_session_metadata_command_attribute
                        AS receipt_attribute
                     WHERE receipt_attribute.command_id = receipt.command_id
                )
                OR EXISTS (
                    SELECT
                        receipt_attribute.attribute_key,
                        receipt_attribute.attribute_value
                      FROM replace_session_metadata_command_attribute
                        AS receipt_attribute
                     WHERE receipt_attribute.command_id = receipt.command_id
                    EXCEPT
                    SELECT
                        current_attribute.attribute_key,
                        current_attribute.attribute_value
                      FROM session_metadata_attribute AS current_attribute
                     WHERE current_attribute.session_id = current.session_id
                )
           )
    ) THEN
        RAISE EXCEPTION
            'current session metadata does not match its source receipt'
            USING ERRCODE = '23514';
    END IF;

    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER session_metadata_matches_receipt
AFTER INSERT OR UPDATE ON session_metadata
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_session_metadata_matches_receipt();

CREATE CONSTRAINT TRIGGER session_metadata_tag_matches_receipt
AFTER INSERT OR UPDATE OR DELETE ON session_metadata_tag
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_session_metadata_matches_receipt();

CREATE CONSTRAINT TRIGGER session_metadata_attribute_matches_receipt
AFTER INSERT OR UPDATE OR DELETE ON session_metadata_attribute
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_session_metadata_matches_receipt();

CREATE FUNCTION reject_sealed_session_metadata_receipt_satellite_insert()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    PERFORM command_id
      FROM durable_command
     WHERE command_id = NEW.command_id
       FOR UPDATE;

    IF NOT FOUND THEN
        RAISE EXCEPTION
            'session metadata receipt satellite % has no command claim',
            NEW.command_id
            USING ERRCODE = '23503';
    END IF;

    IF EXISTS (
        SELECT 1
          FROM replace_session_metadata_command
         WHERE command_id = NEW.command_id
    ) THEN
        RAISE EXCEPTION
            'session metadata receipt % is already sealed',
            NEW.command_id
            USING ERRCODE = '23514';
    END IF;

    RETURN NEW;
END;
$$;

CREATE TRIGGER replace_session_metadata_command_is_append_only
BEFORE UPDATE OR DELETE ON replace_session_metadata_command
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER replace_session_metadata_command_tag_insert_before_seal
BEFORE INSERT ON replace_session_metadata_command_tag
FOR EACH ROW
EXECUTE FUNCTION reject_sealed_session_metadata_receipt_satellite_insert();

CREATE TRIGGER replace_session_metadata_command_tag_is_append_only
BEFORE UPDATE OR DELETE ON replace_session_metadata_command_tag
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER replace_session_metadata_command_attribute_insert_before_seal
BEFORE INSERT ON replace_session_metadata_command_attribute
FOR EACH ROW
EXECUTE FUNCTION reject_sealed_session_metadata_receipt_satellite_insert();

CREATE TRIGGER replace_session_metadata_command_attribute_is_append_only
BEFORE UPDATE OR DELETE ON replace_session_metadata_command_attribute
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE FUNCTION reject_session_metadata_table_truncate()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION
        'session metadata table % is not truncatable',
        TG_TABLE_NAME
        USING ERRCODE = '23514';
END;
$$;

CREATE TRIGGER session_metadata_truncate_is_rejected
BEFORE TRUNCATE ON session_metadata
FOR EACH STATEMENT
EXECUTE FUNCTION reject_session_metadata_table_truncate();

CREATE TRIGGER session_metadata_tag_truncate_is_rejected
BEFORE TRUNCATE ON session_metadata_tag
FOR EACH STATEMENT
EXECUTE FUNCTION reject_session_metadata_table_truncate();

CREATE TRIGGER session_metadata_attribute_truncate_is_rejected
BEFORE TRUNCATE ON session_metadata_attribute
FOR EACH STATEMENT
EXECUTE FUNCTION reject_session_metadata_table_truncate();

CREATE TRIGGER replace_session_metadata_command_truncate_is_rejected
BEFORE TRUNCATE ON replace_session_metadata_command
FOR EACH STATEMENT
EXECUTE FUNCTION reject_session_metadata_table_truncate();

CREATE TRIGGER replace_session_metadata_command_tag_truncate_is_rejected
BEFORE TRUNCATE ON replace_session_metadata_command_tag
FOR EACH STATEMENT
EXECUTE FUNCTION reject_session_metadata_table_truncate();

CREATE TRIGGER replace_session_metadata_command_attribute_truncate_is_rejected
BEFORE TRUNCATE ON replace_session_metadata_command_attribute
FOR EACH STATEMENT
EXECUTE FUNCTION reject_session_metadata_table_truncate();

CREATE OR REPLACE FUNCTION require_durable_command_typed_record()
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
        WHEN 'create_session_from_imported_frontier' THEN
            SELECT count(*)
              INTO matching_records
              FROM create_session_from_imported_frontier_command
             WHERE command_id = NEW.command_id;
        WHEN 'replace_session_defaults' THEN
            SELECT count(*)
              INTO matching_records
              FROM replace_session_defaults_command
             WHERE command_id = NEW.command_id;
        WHEN 'replace_session_metadata' THEN
            SELECT count(*)
              INTO matching_records
              FROM replace_session_metadata_command
             WHERE command_id = NEW.command_id;
        WHEN 'submit_input' THEN
            SELECT count(*)
              INTO matching_records
              FROM submit_input_command
             WHERE command_id = NEW.command_id;
        WHEN 'decide_tool_request' THEN
            SELECT count(*)
              INTO matching_records
              FROM decide_tool_request_command
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
