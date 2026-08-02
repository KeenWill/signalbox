-- Opt-in dotted session placement with append-only history and an explicit
-- root-global-read creation record.

ALTER TABLE durable_command
    DROP CONSTRAINT durable_command_kind_closed,
    DROP CONSTRAINT durable_command_storage_version_supported;

ALTER TABLE durable_command
    ADD CONSTRAINT durable_command_kind_closed CHECK (
        command_kind IN (
            'create_session', 'create_session_from_imported_frontier',
            'replace_session_defaults', 'replace_session_metadata',
            'submit_input', 'decide_tool_request', 'review_workflow',
            'review_orchestration', 'compact_session', 'goal',
            'update_session_placement'
        )
    ),
    ADD CONSTRAINT durable_command_storage_version_supported CHECK (
        (command_kind = 'create_session' AND storage_version IN (1, 2, 3, 4, 5, 6))
        OR (command_kind IN (
            'create_session_from_imported_frontier', 'replace_session_defaults'
        ) AND storage_version IN (1, 2, 3))
        OR (command_kind IN (
            'replace_session_metadata', 'submit_input', 'decide_tool_request',
            'review_workflow', 'review_orchestration', 'compact_session',
            'goal', 'update_session_placement'
        ) AND storage_version = 1)
    );

ALTER TABLE create_session_command
    DROP CONSTRAINT create_session_command_storage_version_supported,
    ADD COLUMN placement_path text,
    ADD COLUMN root_global_read_intent boolean NOT NULL DEFAULT FALSE,
    ADD CONSTRAINT create_session_command_storage_version_supported
        CHECK (storage_version IN (1, 2, 3, 4, 5, 6)),
    ADD CONSTRAINT create_session_command_placement_versioned CHECK (
        storage_version >= 6
        OR (placement_path IS NULL AND NOT root_global_read_intent)
    ),
    ADD CONSTRAINT create_session_command_root_intent_shape CHECK (
        root_global_read_intent = (
            placement_path IS NOT NULL AND position('.' IN placement_path) = 0
        )
    ),
    ADD CONSTRAINT create_session_command_placement_path_shape CHECK (
        placement_path IS NULL
        OR (octet_length(placement_path) BETWEEN 1 AND 4159
            AND placement_path ~ '^[A-Za-z0-9_-]{1,64}(\.[A-Za-z0-9_-]{1,64}){0,63}$')
    );

CREATE TABLE session_placement_event (
    session_id uuid NOT NULL REFERENCES session(session_id) ON DELETE RESTRICT,
    version numeric(20, 0) NOT NULL CHECK (version BETWEEN 1 AND 18446744073709551615),
    prior_version numeric(20, 0) CHECK (prior_version BETWEEN 1 AND 18446744073709551615),
    event_kind text NOT NULL CHECK (event_kind IN ('created', 'updated')),
    placement_path text,
    root_global_read_intent boolean NOT NULL,
    provenance_command_id uuid NOT NULL REFERENCES durable_command(command_id) ON DELETE RESTRICT,
    recorded_at timestamptz NOT NULL,
    PRIMARY KEY (session_id, version),
    UNIQUE (session_id, version, provenance_command_id),
    FOREIGN KEY (session_id, prior_version)
        REFERENCES session_placement_event(session_id, version) ON DELETE RESTRICT,
    CHECK (
        placement_path IS NULL
        OR (octet_length(placement_path) BETWEEN 1 AND 4159
            AND placement_path ~ '^[A-Za-z0-9_-]{1,64}(\.[A-Za-z0-9_-]{1,64}){0,63}$')
    ),
    CHECK (
        (version = 1 AND prior_version IS NULL AND event_kind = 'created')
        OR (version > 1 AND prior_version = version - 1 AND event_kind = 'updated')
    ),
    CHECK (
        root_global_read_intent = (
            placement_path IS NOT NULL AND position('.' IN placement_path) = 0
        )
    )
);

CREATE TABLE session_current_placement (
    session_id uuid PRIMARY KEY REFERENCES session(session_id) ON DELETE RESTRICT,
    current_version numeric(20, 0) NOT NULL CHECK (
        current_version BETWEEN 1 AND 18446744073709551615
    ),
    FOREIGN KEY (session_id, current_version)
        REFERENCES session_placement_event(session_id, version) ON DELETE RESTRICT
);

CREATE TABLE update_session_placement_command (
    command_id uuid PRIMARY KEY,
    command_kind text NOT NULL CHECK (command_kind = 'update_session_placement'),
    storage_version smallint NOT NULL CHECK (storage_version = 1),
    session_id uuid NOT NULL,
    expected_version numeric(20, 0) NOT NULL CHECK (
        expected_version BETWEEN 1 AND 18446744073709551615
    ),
    replacement_path text,
    root_global_read_intent boolean NOT NULL,
    result_kind text NOT NULL CHECK (result_kind IN ('applied', 'rejected')),
    rejection_kind text CHECK (rejection_kind IN (
        'session_not_found', 'current_version_mismatch', 'version_exhausted'
    )),
    result_version numeric(20, 0) CHECK (
        result_version BETWEEN 1 AND 18446744073709551615
    ),
    result_current_version numeric(20, 0) CHECK (
        result_current_version BETWEEN 1 AND 18446744073709551615
    ),
    CHECK (
        replacement_path IS NULL
        OR (octet_length(replacement_path) BETWEEN 1 AND 4159
            AND replacement_path ~ '^[A-Za-z0-9_-]{1,64}(\.[A-Za-z0-9_-]{1,64}){0,63}$')
    ),
    CHECK (
        root_global_read_intent = (
            replacement_path IS NOT NULL AND position('.' IN replacement_path) = 0
        )
    ),
    CONSTRAINT update_session_placement_command_result_shape CHECK (
        (result_kind = 'applied' AND rejection_kind IS NULL
            AND result_version IS NOT NULL
            AND result_version = expected_version + 1
            AND result_current_version IS NULL)
        OR (result_kind = 'rejected' AND rejection_kind = 'session_not_found'
            AND result_version IS NULL AND result_current_version IS NULL)
        OR (result_kind = 'rejected' AND rejection_kind = 'current_version_mismatch'
            AND result_version IS NULL AND result_current_version IS NOT NULL
            AND result_current_version <> expected_version)
        OR (result_kind = 'rejected' AND rejection_kind = 'version_exhausted'
            AND result_version IS NULL
            AND expected_version = 18446744073709551615
            AND result_current_version = 18446744073709551615)
    ),
    FOREIGN KEY (command_id, command_kind, storage_version)
        REFERENCES durable_command(command_id, command_kind, storage_version)
        ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (session_id, result_version, command_id)
        REFERENCES session_placement_event(session_id, version, provenance_command_id)
        ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED
);

CREATE TRIGGER session_placement_event_is_append_only
    BEFORE UPDATE OR DELETE ON session_placement_event
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();
CREATE TRIGGER session_placement_event_reject_truncate
    BEFORE TRUNCATE ON session_placement_event
    FOR EACH STATEMENT EXECUTE FUNCTION reject_outbox_table_truncate();
CREATE TRIGGER update_session_placement_command_is_append_only
    BEFORE UPDATE OR DELETE ON update_session_placement_command
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();
CREATE TRIGGER update_session_placement_command_reject_truncate
    BEFORE TRUNCATE ON update_session_placement_command
    FOR EACH STATEMENT EXECUTE FUNCTION reject_outbox_table_truncate();

CREATE FUNCTION guard_session_placement_head()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'DELETE'
       OR (TG_OP = 'INSERT' AND NEW.current_version <> 1)
       OR (TG_OP = 'UPDATE' AND (
            NEW.session_id <> OLD.session_id
            OR NEW.current_version <> OLD.current_version + 1
       )) THEN
        RAISE EXCEPTION 'session placement head must advance by one version';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER session_placement_head_is_guarded
    BEFORE INSERT OR UPDATE OR DELETE ON session_current_placement
    FOR EACH ROW EXECUTE FUNCTION guard_session_placement_head();
CREATE TRIGGER session_placement_head_reject_truncate
    BEFORE TRUNCATE ON session_current_placement
    FOR EACH STATEMENT EXECUTE FUNCTION reject_outbox_table_truncate();

WITH creation AS (
    SELECT created_session_id AS session_id, command_id FROM create_session_command
    UNION ALL
    SELECT created_session_id, command_id
      FROM create_session_from_imported_frontier_command
)
INSERT INTO session_placement_event
    (session_id, version, prior_version, event_kind, placement_path,
     root_global_read_intent, provenance_command_id, recorded_at)
SELECT session_id, 1, NULL, 'created', NULL, FALSE, command_id,
       transaction_timestamp()
  FROM creation;

INSERT INTO session_current_placement (session_id, current_version)
SELECT session_id, 1 FROM session_placement_event;

CREATE OR REPLACE FUNCTION require_durable_command_typed_record()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE matching_records bigint;
BEGIN
    IF NEW.command_kind <> 'review_orchestration' AND EXISTS (
        SELECT 1 FROM review_orchestration_command_recovery
         WHERE command_id = NEW.command_id
    ) THEN
        RAISE EXCEPTION 'durable command % is reserved by review orchestration recovery', NEW.command_id
            USING ERRCODE = '23505';
    END IF;
    CASE NEW.command_kind
        WHEN 'create_session' THEN SELECT count(*) INTO matching_records FROM create_session_command WHERE command_id = NEW.command_id;
        WHEN 'create_session_from_imported_frontier' THEN SELECT count(*) INTO matching_records FROM create_session_from_imported_frontier_command WHERE command_id = NEW.command_id;
        WHEN 'replace_session_defaults' THEN SELECT count(*) INTO matching_records FROM replace_session_defaults_command WHERE command_id = NEW.command_id;
        WHEN 'replace_session_metadata' THEN SELECT count(*) INTO matching_records FROM replace_session_metadata_command WHERE command_id = NEW.command_id;
        WHEN 'submit_input' THEN SELECT count(*) INTO matching_records FROM submit_input_command WHERE command_id = NEW.command_id;
        WHEN 'decide_tool_request' THEN SELECT count(*) INTO matching_records FROM decide_tool_request_command WHERE command_id = NEW.command_id;
        WHEN 'review_workflow' THEN SELECT count(*) INTO matching_records FROM review_workflow_command WHERE command_id = NEW.command_id;
        WHEN 'review_orchestration' THEN SELECT (SELECT count(*) FROM review_orchestration_command WHERE command_id = NEW.command_id) + (SELECT count(*) FROM review_orchestration_command_intent WHERE command_id = NEW.command_id) INTO matching_records;
        WHEN 'compact_session' THEN SELECT count(*) INTO matching_records FROM compact_session_command WHERE command_id = NEW.command_id;
        WHEN 'goal' THEN SELECT count(*) INTO matching_records FROM goal_command WHERE command_id = NEW.command_id;
        WHEN 'update_session_placement' THEN SELECT count(*) INTO matching_records FROM update_session_placement_command WHERE command_id = NEW.command_id;
        ELSE RAISE EXCEPTION 'unsupported durable command kind %', NEW.command_kind USING ERRCODE = '23514';
    END CASE;
    IF matching_records <> 1 THEN
        RAISE EXCEPTION 'durable command % requires exactly one % typed record', NEW.command_id, NEW.command_kind USING ERRCODE = '23503';
    END IF;
    RETURN NULL;
END;
$$;
