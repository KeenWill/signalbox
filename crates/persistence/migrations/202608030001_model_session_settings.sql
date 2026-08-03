-- Durable model/session settings snapshots and change evidence.

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

ALTER TABLE replace_session_defaults_command
    ADD COLUMN replacement_model_settings jsonb NOT NULL DEFAULT
        '{"precedence":{"per_call":{"reasoning_level":{"kind":"inherit"},"fast_mode":{"kind":"inherit"},"service_tier":{"kind":"inherit"}},"session":{"reasoning_level":{"kind":"inherit"},"fast_mode":{"kind":"inherit"},"service_tier":{"kind":"inherit"}},"profile":{"reasoning_level":{"kind":"inherit"},"fast_mode":{"kind":"inherit"},"service_tier":{"kind":"inherit"}},"global_default":{"reasoning_level":{"kind":"inherit"},"fast_mode":{"kind":"inherit"},"service_tier":{"kind":"inherit"}}},"effective":{"reasoning_level":null,"fast_mode":"disabled","service_tier":null},"reasoning_source":null,"fast_mode_source":null,"service_tier_source":null,"validated_for_selection_id":null}'::jsonb,
    ADD COLUMN caller_model_settings jsonb NOT NULL DEFAULT
        '{"reasoning_level":{"kind":"inherit"},"fast_mode":{"kind":"inherit"},"service_tier":{"kind":"inherit"}}'::jsonb,
    ADD CONSTRAINT replace_session_defaults_replacement_model_settings_object
        CHECK (jsonb_typeof(replacement_model_settings) = 'object'),
    ADD CONSTRAINT replace_session_defaults_caller_model_settings_object
        CHECK (jsonb_typeof(caller_model_settings) = 'object');

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
        FOREIGN KEY (command_id)
        REFERENCES replace_session_defaults_command (command_id)
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
    adjustments jsonb NOT NULL,

    CONSTRAINT turn_model_settings_resolved_documents
        CHECK (
            jsonb_typeof(per_call_model_settings) = 'object'
            AND jsonb_typeof(resolved_model_settings) = 'object'
            AND jsonb_typeof(adjustments) = 'array'
        ),
    CONSTRAINT turn_model_settings_resolved_input_fk
        FOREIGN KEY (accepted_input_id)
        REFERENCES accepted_input (accepted_input_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CONSTRAINT turn_model_settings_resolved_defaults_fk
        FOREIGN KEY (session_id, defaults_version)
        REFERENCES session_defaults_version (session_id, version)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
);

CREATE TRIGGER session_model_settings_changed_is_append_only
BEFORE UPDATE OR DELETE ON session_model_settings_changed
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER turn_model_settings_resolved_is_append_only
BEFORE UPDATE OR DELETE ON turn_model_settings_resolved
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();
