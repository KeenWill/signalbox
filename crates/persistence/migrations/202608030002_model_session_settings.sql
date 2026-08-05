-- Durable model/session settings snapshots and change evidence.

-- Settings-bearing command payloads advance their owning representations so
-- the defaults backfilled below cannot be reinterpreted as caller-supplied
-- settings on a legacy command.
ALTER TABLE durable_command
    DROP CONSTRAINT durable_command_storage_version_supported,
    ADD CONSTRAINT durable_command_storage_version_supported CHECK (
        (command_kind = 'create_session'
            AND storage_version IN (1, 2, 3, 4, 5, 6, 7))
        OR (command_kind IN (
            'replace_session_defaults'
        ) AND storage_version IN (1, 2, 3, 4))
        OR (command_kind = 'create_session_from_imported_frontier'
            AND storage_version IN (1, 2, 3, 5))
        OR (command_kind = 'submit_input' AND storage_version IN (1, 2))
        OR (command_kind IN (
            'replace_session_metadata', 'decide_tool_request',
            'review_workflow', 'review_orchestration', 'compact_session',
            'goal', 'update_session_placement'
        ) AND storage_version = 1)
    );

ALTER TABLE create_session_command
    DROP CONSTRAINT create_session_command_storage_version_supported,
    ADD CONSTRAINT create_session_command_storage_version_supported
        CHECK (storage_version IN (1, 2, 3, 4, 5, 6, 7));

ALTER TABLE create_session_from_imported_frontier_command
    DROP CONSTRAINT
        create_session_from_imported_frontier_command_version_supported,
    ADD CONSTRAINT
        create_session_from_imported_frontier_command_version_supported
        CHECK (storage_version IN (1, 2, 3, 5));

ALTER TABLE replace_session_defaults_command
    DROP CONSTRAINT replace_session_defaults_command_storage_version_supported,
    ADD CONSTRAINT replace_session_defaults_command_storage_version_supported
        CHECK (storage_version IN (1, 2, 3, 4));

ALTER TABLE submit_input_command
    DROP CONSTRAINT submit_input_command_storage_version_supported,
    ADD CONSTRAINT submit_input_command_storage_version_supported
        CHECK (storage_version IN (1, 2));

ALTER TABLE session_defaults_version
    ADD COLUMN model_settings jsonb NOT NULL DEFAULT
        '{"precedence":{"per_call":{"reasoning_level":{"kind":"inherit"},"fast_mode":{"kind":"inherit"},"service_tier":{"kind":"inherit"}},"session":{"reasoning_level":{"kind":"inherit"},"fast_mode":{"kind":"inherit"},"service_tier":{"kind":"inherit"}},"profile":{"reasoning_level":{"kind":"inherit"},"fast_mode":{"kind":"inherit"},"service_tier":{"kind":"inherit"}},"global_default":{"reasoning_level":{"kind":"inherit"},"fast_mode":{"kind":"inherit"},"service_tier":{"kind":"inherit"}}},"effective":{"reasoning_level":null,"fast_mode":"disabled","service_tier":null},"reasoning_source":null,"fast_mode_source":null,"service_tier_source":null,"validated_for_selection_id":null}'::jsonb,
    ADD CONSTRAINT session_defaults_version_model_settings_object
        CHECK (jsonb_typeof(model_settings) = 'object');

ALTER TABLE accepted_input
    ADD COLUMN model_settings_override jsonb NOT NULL DEFAULT
        '{"reasoning_level":{"kind":"inherit"},"fast_mode":{"kind":"inherit"},"service_tier":{"kind":"inherit"}}'::jsonb,
    ADD CONSTRAINT accepted_input_model_settings_override_object
        CHECK (jsonb_typeof(model_settings_override) = 'object');

-- Existing configuration roots predate per-turn settings evidence. New roots
-- require it; reclassified turns resolve through the recursive chain to the
-- original root and therefore inherit the correct side of this cutover.
ALTER TABLE queued_input_origin
    ADD COLUMN model_settings_evidence_required boolean NOT NULL DEFAULT FALSE;

ALTER TABLE queued_input_origin
    ALTER COLUMN model_settings_evidence_required SET DEFAULT TRUE;

ALTER TABLE submit_input_command
    ADD COLUMN model_settings_override jsonb NOT NULL DEFAULT
        '{"reasoning_level":{"kind":"inherit"},"fast_mode":{"kind":"inherit"},"service_tier":{"kind":"inherit"}}'::jsonb,
    ADD CONSTRAINT submit_input_command_model_settings_override_object
        CHECK (jsonb_typeof(model_settings_override) = 'object');

ALTER TABLE replace_session_defaults_command
    ADD COLUMN replacement_model_settings jsonb NOT NULL DEFAULT
        '{"precedence":{"per_call":{"reasoning_level":{"kind":"inherit"},"fast_mode":{"kind":"inherit"},"service_tier":{"kind":"inherit"}},"session":{"reasoning_level":{"kind":"inherit"},"fast_mode":{"kind":"inherit"},"service_tier":{"kind":"inherit"}},"profile":{"reasoning_level":{"kind":"inherit"},"fast_mode":{"kind":"inherit"},"service_tier":{"kind":"inherit"}},"global_default":{"reasoning_level":{"kind":"inherit"},"fast_mode":{"kind":"inherit"},"service_tier":{"kind":"inherit"}}},"effective":{"reasoning_level":null,"fast_mode":"disabled","service_tier":null},"reasoning_source":null,"fast_mode_source":null,"service_tier_source":null,"validated_for_selection_id":null}'::jsonb,
    ADD COLUMN caller_model_settings jsonb NOT NULL DEFAULT
        '{"reasoning_level":{"kind":"inherit"},"fast_mode":{"kind":"inherit"},"service_tier":{"kind":"inherit"}}'::jsonb,
    ADD CONSTRAINT replace_session_defaults_replacement_model_settings_object
        CHECK (jsonb_typeof(replacement_model_settings) = 'object'),
    ADD CONSTRAINT replace_session_defaults_caller_model_settings_object
        CHECK (jsonb_typeof(caller_model_settings) = 'object'),
    ADD CONSTRAINT replace_session_defaults_command_settings_event_key
        UNIQUE (command_id, result_session_id, result_installed_version);

ALTER TABLE create_session_command
    ADD COLUMN model_settings jsonb NOT NULL DEFAULT
        '{"precedence":{"per_call":{"reasoning_level":{"kind":"inherit"},"fast_mode":{"kind":"inherit"},"service_tier":{"kind":"inherit"}},"session":{"reasoning_level":{"kind":"inherit"},"fast_mode":{"kind":"inherit"},"service_tier":{"kind":"inherit"}},"profile":{"reasoning_level":{"kind":"inherit"},"fast_mode":{"kind":"inherit"},"service_tier":{"kind":"inherit"}},"global_default":{"reasoning_level":{"kind":"inherit"},"fast_mode":{"kind":"inherit"},"service_tier":{"kind":"inherit"}}},"effective":{"reasoning_level":null,"fast_mode":"disabled","service_tier":null},"reasoning_source":null,"fast_mode_source":null,"service_tier_source":null,"validated_for_selection_id":null}'::jsonb,
    ADD CONSTRAINT create_session_command_model_settings_object
        CHECK (jsonb_typeof(model_settings) = 'object');

ALTER TABLE create_session_from_imported_frontier_command
    ADD COLUMN model_settings jsonb NOT NULL DEFAULT
        '{"precedence":{"per_call":{"reasoning_level":{"kind":"inherit"},"fast_mode":{"kind":"inherit"},"service_tier":{"kind":"inherit"}},"session":{"reasoning_level":{"kind":"inherit"},"fast_mode":{"kind":"inherit"},"service_tier":{"kind":"inherit"}},"profile":{"reasoning_level":{"kind":"inherit"},"fast_mode":{"kind":"inherit"},"service_tier":{"kind":"inherit"}},"global_default":{"reasoning_level":{"kind":"inherit"},"fast_mode":{"kind":"inherit"},"service_tier":{"kind":"inherit"}}},"effective":{"reasoning_level":null,"fast_mode":"disabled","service_tier":null},"reasoning_source":null,"fast_mode_source":null,"service_tier_source":null,"validated_for_selection_id":null}'::jsonb,
    ADD CONSTRAINT imported_session_command_model_settings_object
        CHECK (jsonb_typeof(model_settings) = 'object');

CREATE TABLE session_model_settings_changed (
    session_id uuid NOT NULL,
    command_id uuid NOT NULL UNIQUE,
    prior_defaults_version numeric(20, 0) NOT NULL,
    installed_defaults_version numeric(20, 0) NOT NULL,
    prior_model_settings jsonb NOT NULL,
    installed_model_settings jsonb NOT NULL,
    caller_model_settings jsonb NOT NULL,
    adjustments jsonb NOT NULL,

    CONSTRAINT session_model_settings_changed_pk
        PRIMARY KEY (session_id, installed_defaults_version),
    CONSTRAINT session_model_settings_changed_successor
        CHECK (installed_defaults_version = prior_defaults_version + 1),
    CONSTRAINT session_model_settings_changed_documents
        CHECK (
            jsonb_typeof(prior_model_settings) = 'object'
            AND jsonb_typeof(installed_model_settings) = 'object'
            AND jsonb_typeof(caller_model_settings) = 'object'
            AND jsonb_typeof(adjustments) = 'array'
        ),
    CONSTRAINT session_model_settings_changed_command_fk
        FOREIGN KEY (command_id, session_id, installed_defaults_version)
        REFERENCES replace_session_defaults_command (
            command_id,
            result_session_id,
            result_installed_version
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT session_model_settings_changed_prior_fk
        FOREIGN KEY (session_id, prior_defaults_version)
        REFERENCES session_defaults_version (session_id, version)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CONSTRAINT session_model_settings_changed_installed_fk
        FOREIGN KEY (session_id, installed_defaults_version)
        REFERENCES session_defaults_version (session_id, version)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
);

CREATE TABLE turn_model_settings_resolved (
    accepted_input_id uuid PRIMARY KEY,
    turn_id uuid NOT NULL UNIQUE,
    session_id uuid NOT NULL,
    defaults_version numeric(20, 0) NOT NULL,
    selected_direct_model_id uuid NOT NULL,
    per_call_model_settings jsonb NOT NULL,
    resolved_model_settings jsonb NOT NULL,
    adjusted_from_selection_id uuid,
    adjustments jsonb NOT NULL,

    CONSTRAINT turn_model_settings_resolved_session_key
        UNIQUE (accepted_input_id, session_id),
    CONSTRAINT turn_model_settings_resolved_documents
        CHECK (
            jsonb_typeof(per_call_model_settings) = 'object'
            AND jsonb_typeof(resolved_model_settings) = 'object'
            AND jsonb_typeof(adjustments) = 'array'
        ),
    CONSTRAINT turn_model_settings_resolved_adjustment_source
        CHECK (
            (
                adjusted_from_selection_id IS NULL
                AND adjustments = '[]'::jsonb
            )
            OR (
                adjusted_from_selection_id IS NOT NULL
                AND adjusted_from_selection_id <> selected_direct_model_id
                AND jsonb_array_length(adjustments) > 0
            )
        ),
    CONSTRAINT turn_model_settings_resolved_input_fk
        FOREIGN KEY (accepted_input_id, session_id, turn_id)
        REFERENCES accepted_input (
            accepted_input_id,
            session_id,
            origin_turn_id
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT turn_model_settings_resolved_turn_fk
        FOREIGN KEY (turn_id, session_id)
        REFERENCES turn_lifecycle (turn_id, session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT turn_model_settings_resolved_defaults_fk
        FOREIGN KEY (session_id, defaults_version)
        REFERENCES session_defaults_version (session_id, version)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
);

ALTER TABLE outbox_event
    DROP CONSTRAINT outbox_event_kind_closed;

ALTER TABLE outbox_event
    ADD CONSTRAINT outbox_event_kind_closed
        CHECK (
            event_kind IN (
                'session_created',
                'session_model_settings_changed',
                'turn_model_settings_resolved',
                'input_accepted',
                'goal_turn_retired',
                'turn_activated',
                'turn_failed',
                'model_call_transition',
                'tool_batch_transition',
                'tool_approval_decided',
                'context_compacted',
                'turn_completed',
                'turn_refused',
                'turn_cancelled',
                'turn_reconciliation_required'
            )
        );

CREATE TABLE session_model_settings_changed_outbox_event (
    event_sequence numeric(20, 0) PRIMARY KEY,
    event_kind text NOT NULL,
    storage_version smallint NOT NULL,
    session_id uuid NOT NULL,
    installed_defaults_version numeric(20, 0) NOT NULL,

    CONSTRAINT session_model_settings_changed_outbox_kind_closed
        CHECK (event_kind = 'session_model_settings_changed'),
    CONSTRAINT session_model_settings_changed_outbox_version_supported
        CHECK (storage_version = 1),
    CONSTRAINT session_model_settings_changed_outbox_header_fk
        FOREIGN KEY (event_sequence, event_kind, storage_version, session_id)
        REFERENCES outbox_event (
            event_sequence,
            event_kind,
            storage_version,
            session_id
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT session_model_settings_changed_outbox_event_fk
        FOREIGN KEY (session_id, installed_defaults_version)
        REFERENCES session_model_settings_changed (
            session_id,
            installed_defaults_version
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE TABLE turn_model_settings_resolved_outbox_event (
    event_sequence numeric(20, 0) PRIMARY KEY,
    event_kind text NOT NULL,
    storage_version smallint NOT NULL,
    session_id uuid NOT NULL,
    accepted_input_id uuid NOT NULL UNIQUE,

    CONSTRAINT turn_model_settings_resolved_outbox_kind_closed
        CHECK (event_kind = 'turn_model_settings_resolved'),
    CONSTRAINT turn_model_settings_resolved_outbox_version_supported
        CHECK (storage_version = 1),
    CONSTRAINT turn_model_settings_resolved_outbox_header_fk
        FOREIGN KEY (event_sequence, event_kind, storage_version, session_id)
        REFERENCES outbox_event (
            event_sequence,
            event_kind,
            storage_version,
            session_id
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT turn_model_settings_resolved_outbox_event_fk
        FOREIGN KEY (accepted_input_id, session_id)
        REFERENCES turn_model_settings_resolved (
            accepted_input_id,
            session_id
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE TRIGGER session_model_settings_changed_outbox_event_is_append_only
BEFORE UPDATE OR DELETE ON session_model_settings_changed_outbox_event
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER session_model_settings_changed_outbox_event_cannot_be_truncated
BEFORE TRUNCATE ON session_model_settings_changed_outbox_event
FOR EACH STATEMENT
EXECUTE FUNCTION reject_outbox_table_truncate();

CREATE TRIGGER turn_model_settings_resolved_outbox_event_is_append_only
BEFORE UPDATE OR DELETE ON turn_model_settings_resolved_outbox_event
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER turn_model_settings_resolved_outbox_event_cannot_be_truncated
BEFORE TRUNCATE ON turn_model_settings_resolved_outbox_event
FOR EACH STATEMENT
EXECUTE FUNCTION reject_outbox_table_truncate();

CREATE OR REPLACE FUNCTION require_outbox_event_typed_record()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    matching_records bigint;
BEGIN
    CASE NEW.event_kind
        WHEN 'session_created' THEN
            SELECT count(*) INTO matching_records
              FROM session_created_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'session_model_settings_changed' THEN
            SELECT count(*) INTO matching_records
              FROM session_model_settings_changed_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'turn_model_settings_resolved' THEN
            SELECT count(*) INTO matching_records
              FROM turn_model_settings_resolved_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'input_accepted' THEN
            SELECT count(*) INTO matching_records
              FROM input_accepted_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'goal_turn_retired' THEN
            SELECT count(*) INTO matching_records
              FROM goal_turn_retired_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'turn_activated' THEN
            SELECT count(*) INTO matching_records
              FROM turn_activated_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'turn_failed' THEN
            SELECT count(*) INTO matching_records
              FROM turn_failed_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'model_call_transition' THEN
            SELECT count(*) INTO matching_records
              FROM model_call_transition_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'tool_batch_transition' THEN
            SELECT count(*) INTO matching_records
              FROM tool_batch_transition_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'tool_approval_decided' THEN
            SELECT count(*) INTO matching_records
              FROM tool_approval_decided_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'context_compacted' THEN
            SELECT count(*) INTO matching_records
              FROM context_compacted_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'turn_completed' THEN
            SELECT count(*) INTO matching_records
              FROM turn_completed_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'turn_refused' THEN
            SELECT count(*) INTO matching_records
              FROM turn_refused_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'turn_cancelled' THEN
            SELECT count(*) INTO matching_records
              FROM turn_cancelled_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'turn_reconciliation_required' THEN
            SELECT count(*) INTO matching_records
              FROM turn_reconciliation_required_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        ELSE
            RAISE EXCEPTION 'unsupported outbox event kind %', NEW.event_kind
                USING ERRCODE = '23514';
    END CASE;

    IF matching_records <> 1 THEN
        RAISE EXCEPTION 'outbox event % requires exactly one % typed record',
            NEW.event_sequence,
            NEW.event_kind
            USING ERRCODE = '23503';
    END IF;
    RETURN NULL;
END;
$$;

CREATE TRIGGER session_model_settings_changed_is_append_only
BEFORE UPDATE OR DELETE ON session_model_settings_changed
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER turn_model_settings_resolved_is_append_only
BEFORE UPDATE OR DELETE ON turn_model_settings_resolved
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();
