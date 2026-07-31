-- Retain exact orchestration command identity across the aggregate-effect boundary.

ALTER TABLE review_orchestration_concern_claim
    ADD COLUMN effect_sequence bigint GENERATED ALWAYS AS IDENTITY;

ALTER TABLE review_orchestration_concern_claim
    ADD CONSTRAINT review_orchestration_concern_claim_effect_sequence_unique
    UNIQUE (effect_sequence);

CREATE TABLE review_orchestration_command_intent (
    command_id uuid PRIMARY KEY,
    command_kind text NOT NULL CHECK (command_kind = 'review_orchestration'),
    storage_version smallint NOT NULL CHECK (storage_version = 1),
    semantic_digest bytea NOT NULL CHECK (octet_length(semantic_digest) = 32),
    attempt_id uuid NOT NULL,
    operation_kind text NOT NULL CHECK (operation_kind IN
        ('start', 'import', 'concern', 'judgment_plan', 'judgment_effect', 'repair', 'publication')),
    FOREIGN KEY (command_id, command_kind, storage_version)
        REFERENCES durable_command(command_id, command_kind, storage_version)
        ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED
);

CREATE TABLE review_orchestration_command_effect (
    command_id uuid PRIMARY KEY,
    attempt_id uuid NOT NULL REFERENCES review_orchestration_attempt(attempt_id)
        ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    operation_kind text NOT NULL CHECK (operation_kind IN
        ('start', 'import', 'concern', 'judgment_plan', 'judgment_effect', 'repair', 'publication')),
    FOREIGN KEY (command_id)
        REFERENCES durable_command(command_id) ON DELETE RESTRICT
);

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
        WHEN 'review_orchestration' THEN
            SELECT
                (SELECT count(*) FROM review_orchestration_command WHERE command_id = NEW.command_id)
                + (SELECT count(*) FROM review_orchestration_command_intent WHERE command_id = NEW.command_id)
            INTO matching_records;
        WHEN 'compact_session' THEN SELECT count(*) INTO matching_records FROM compact_session_command WHERE command_id = NEW.command_id;
        ELSE RAISE EXCEPTION 'unsupported durable command kind %', NEW.command_kind USING ERRCODE = '23514';
    END CASE;
    IF matching_records <> 1 THEN
        RAISE EXCEPTION 'durable command % requires exactly one % typed record', NEW.command_id, NEW.command_kind USING ERRCODE = '23503';
    END IF;
    RETURN NULL;
END;
$$;

CREATE OR REPLACE FUNCTION reject_review_orchestration_intent_change()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'UPDATE' THEN
        RAISE EXCEPTION 'review orchestration command intent is immutable'
            USING ERRCODE = '23514';
    END IF;
    IF NOT EXISTS (
        SELECT 1 FROM review_orchestration_command WHERE command_id = OLD.command_id
    ) THEN
        RAISE EXCEPTION 'review orchestration command intent requires its replacement receipt'
            USING ERRCODE = '23503';
    END IF;
    RETURN OLD;
END;
$$;

CREATE TRIGGER review_orchestration_command_effect_is_append_only
    BEFORE UPDATE OR DELETE ON review_orchestration_command_effect
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER review_orchestration_command_effect_reject_truncate
    BEFORE TRUNCATE ON review_orchestration_command_effect
    FOR EACH STATEMENT EXECUTE FUNCTION reject_review_workflow_truncate();

CREATE TRIGGER review_orchestration_command_intent_change_guard
    BEFORE UPDATE OR DELETE ON review_orchestration_command_intent
    FOR EACH ROW EXECUTE FUNCTION reject_review_orchestration_intent_change();

CREATE TRIGGER review_orchestration_command_intent_reject_truncate
    BEFORE TRUNCATE ON review_orchestration_command_intent
    FOR EACH STATEMENT EXECUTE FUNCTION reject_review_workflow_truncate();
