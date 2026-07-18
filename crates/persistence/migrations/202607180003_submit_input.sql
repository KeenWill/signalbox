-- Typed durable input acceptance before turn-lifecycle implementation.
--
-- Applied StartWhenNoActiveTurn commands commit immutable accepted-input and
-- queued-origin facts. Active-work delivery variants and other authoritative
-- failures commit only their typed terminal result.

ALTER TABLE durable_command
    DROP CONSTRAINT durable_command_kind_closed;

ALTER TABLE durable_command
    ADD CONSTRAINT durable_command_kind_closed
    CHECK (
        command_kind IN (
            'create_session',
            'replace_session_defaults',
            'submit_input'
        )
    );

CREATE TABLE submit_input_command (
    command_id uuid PRIMARY KEY,
    command_kind text NOT NULL,
    storage_version smallint NOT NULL,
    session_id uuid NOT NULL,
    actor_kind text NOT NULL,
    actor_turn_id uuid,
    actor_tool_request_id uuid,
    content_kind text NOT NULL,
    content_text text NOT NULL,
    delivery_kind text NOT NULL,
    expected_active_turn_id uuid,
    expected_defaults_version numeric(20, 0),
    model_override_kind text,
    replacement_model_kind text,
    replacement_direct_model_selection_id uuid,
    replacement_model_alias_id uuid,
    result_kind text NOT NULL,
    rejection_kind text,
    result_session_id uuid NOT NULL,
    result_accepted_input_id uuid,
    result_turn_id uuid,
    result_expected_active_turn_id uuid,
    result_expected_defaults_version numeric(20, 0),
    result_current_defaults_version numeric(20, 0),
    result_unknown_alias_id uuid,
    result_selected_defaults_version numeric(20, 0),
    result_last_position numeric(20, 0),

    CONSTRAINT submit_input_command_kind_closed
        CHECK (command_kind = 'submit_input'),
    CONSTRAINT submit_input_command_storage_version_supported
        CHECK (storage_version = 1),
    CONSTRAINT submit_input_command_actor_kind_closed
        CHECK (actor_kind IN ('owner', 'model', 'recovery', 'tool')),
    CONSTRAINT submit_input_command_actor_shape
        CHECK (
            (
                actor_kind IN ('owner', 'recovery')
                AND actor_turn_id IS NULL
                AND actor_tool_request_id IS NULL
            )
            OR
            (
                actor_kind = 'model'
                AND actor_turn_id IS NOT NULL
                AND actor_tool_request_id IS NULL
            )
            OR
            (
                actor_kind = 'tool'
                AND actor_turn_id IS NULL
                AND actor_tool_request_id IS NOT NULL
            )
        ),
    CONSTRAINT submit_input_command_content_kind_closed
        CHECK (content_kind = 'text'),
    CONSTRAINT submit_input_command_content_nonempty
        CHECK (char_length(content_text) > 0),
    CONSTRAINT submit_input_command_delivery_kind_closed
        CHECK (
            delivery_kind IN (
                'start_when_no_active_turn',
                'interrupt',
                'next_safe_point',
                'after_current_turn'
            )
        ),
    CONSTRAINT submit_input_command_expected_defaults_positive_u64
        CHECK (
            expected_defaults_version IS NULL
            OR (
                expected_defaults_version >= 1
                AND expected_defaults_version <= 18446744073709551615
            )
        ),
    CONSTRAINT submit_input_command_configuration_shape
        CHECK (
            (
                model_override_kind IS NULL
                AND replacement_model_kind IS NULL
                AND replacement_direct_model_selection_id IS NULL
                AND replacement_model_alias_id IS NULL
            )
            OR
            (
                model_override_kind = 'use_session_default'
                AND replacement_model_kind IS NULL
                AND replacement_direct_model_selection_id IS NULL
                AND replacement_model_alias_id IS NULL
            )
            OR
            (
                model_override_kind = 'replace_with'
                AND replacement_model_kind = 'direct'
                AND replacement_direct_model_selection_id IS NOT NULL
                AND replacement_model_alias_id IS NULL
            )
            OR
            (
                model_override_kind = 'replace_with'
                AND replacement_model_kind = 'alias'
                AND replacement_direct_model_selection_id IS NULL
                AND replacement_model_alias_id IS NOT NULL
            )
        ),
    CONSTRAINT submit_input_command_delivery_shape
        CHECK (
            (
                delivery_kind = 'start_when_no_active_turn'
                AND expected_active_turn_id IS NULL
                AND expected_defaults_version IS NOT NULL
                AND model_override_kind IS NOT NULL
            )
            OR
            (
                delivery_kind IN ('interrupt', 'after_current_turn')
                AND expected_active_turn_id IS NOT NULL
                AND expected_defaults_version IS NOT NULL
                AND model_override_kind IS NOT NULL
            )
            OR
            (
                delivery_kind = 'next_safe_point'
                AND expected_active_turn_id IS NOT NULL
                AND expected_defaults_version IS NULL
                AND model_override_kind IS NULL
            )
        ),
    CONSTRAINT submit_input_command_result_kind_closed
        CHECK (result_kind IN ('applied', 'rejected')),
    CONSTRAINT submit_input_command_rejection_kind_closed
        CHECK (
            rejection_kind IS NULL
            OR rejection_kind IN (
                'session_not_found',
                'no_active_turn',
                'session_defaults_version_mismatch',
                'unknown_model_alias',
                'acceptance_position_exhausted'
            )
        ),
    CONSTRAINT submit_input_command_result_session_matches
        CHECK (result_session_id = session_id),
    CONSTRAINT submit_input_command_result_expected_defaults_positive_u64
        CHECK (
            result_expected_defaults_version IS NULL
            OR (
                result_expected_defaults_version >= 1
                AND result_expected_defaults_version <= 18446744073709551615
            )
        ),
    CONSTRAINT submit_input_command_result_current_defaults_positive_u64
        CHECK (
            result_current_defaults_version IS NULL
            OR (
                result_current_defaults_version >= 1
                AND result_current_defaults_version <= 18446744073709551615
            )
        ),
    CONSTRAINT submit_input_command_result_selected_defaults_positive_u64
        CHECK (
            result_selected_defaults_version IS NULL
            OR (
                result_selected_defaults_version >= 1
                AND result_selected_defaults_version <= 18446744073709551615
            )
        ),
    CONSTRAINT submit_input_command_result_last_position_positive_u64
        CHECK (
            result_last_position IS NULL
            OR (
                result_last_position >= 1
                AND result_last_position <= 18446744073709551615
            )
        ),
    CONSTRAINT submit_input_command_result_shape
        CHECK (
            (
                result_kind = 'applied'
                AND rejection_kind IS NULL
                AND result_accepted_input_id IS NOT NULL
                AND result_turn_id IS NOT NULL
                AND result_expected_active_turn_id IS NULL
                AND result_expected_defaults_version IS NULL
                AND result_current_defaults_version IS NULL
                AND result_unknown_alias_id IS NULL
                AND result_selected_defaults_version IS NULL
                AND result_last_position IS NULL
            )
            OR
            (
                result_kind = 'rejected'
                AND rejection_kind = 'session_not_found'
                AND result_accepted_input_id IS NULL
                AND result_turn_id IS NULL
                AND result_expected_active_turn_id IS NULL
                AND result_expected_defaults_version IS NULL
                AND result_current_defaults_version IS NULL
                AND result_unknown_alias_id IS NULL
                AND result_selected_defaults_version IS NULL
                AND result_last_position IS NULL
            )
            OR
            (
                result_kind = 'rejected'
                AND rejection_kind = 'no_active_turn'
                AND result_accepted_input_id IS NULL
                AND result_turn_id IS NULL
                AND result_expected_active_turn_id = expected_active_turn_id
                AND result_expected_defaults_version IS NULL
                AND result_current_defaults_version IS NULL
                AND result_unknown_alias_id IS NULL
                AND result_selected_defaults_version IS NULL
                AND result_last_position IS NULL
            )
            OR
            (
                result_kind = 'rejected'
                AND rejection_kind = 'session_defaults_version_mismatch'
                AND result_accepted_input_id IS NULL
                AND result_turn_id IS NULL
                AND result_expected_active_turn_id IS NULL
                AND result_expected_defaults_version = expected_defaults_version
                AND result_current_defaults_version IS NOT NULL
                AND result_current_defaults_version <> result_expected_defaults_version
                AND result_unknown_alias_id IS NULL
                AND result_selected_defaults_version IS NULL
                AND result_last_position IS NULL
            )
            OR
            (
                result_kind = 'rejected'
                AND rejection_kind = 'unknown_model_alias'
                AND result_accepted_input_id IS NULL
                AND result_turn_id IS NULL
                AND result_expected_active_turn_id IS NULL
                AND result_expected_defaults_version IS NULL
                AND result_current_defaults_version IS NULL
                AND result_unknown_alias_id IS NOT NULL
                AND result_selected_defaults_version = expected_defaults_version
                AND result_last_position IS NULL
            )
            OR
            (
                result_kind = 'rejected'
                AND rejection_kind = 'acceptance_position_exhausted'
                AND result_accepted_input_id IS NULL
                AND result_turn_id IS NULL
                AND result_expected_active_turn_id IS NULL
                AND result_expected_defaults_version IS NULL
                AND result_current_defaults_version IS NULL
                AND result_unknown_alias_id IS NULL
                AND result_selected_defaults_version IS NULL
                AND result_last_position = 18446744073709551615
            )
        ),
    CONSTRAINT submit_input_command_registry_fk
        FOREIGN KEY (command_id, command_kind, storage_version)
        REFERENCES durable_command (
            command_id,
            command_kind,
            storage_version
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT submit_input_command_selected_defaults_fk
        FOREIGN KEY (result_session_id, result_selected_defaults_version)
        REFERENCES session_defaults_version (session_id, version)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT submit_input_command_current_defaults_fk
        FOREIGN KEY (result_session_id, result_current_defaults_version)
        REFERENCES session_defaults_version (session_id, version)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT submit_input_command_result_correlation_key
        UNIQUE (
            command_id,
            result_accepted_input_id,
            result_session_id,
            result_turn_id
        )
);

CREATE TABLE accepted_input (
    accepted_input_id uuid PRIMARY KEY,
    accepting_command_id uuid NOT NULL UNIQUE,
    session_id uuid NOT NULL,
    content_kind text NOT NULL,
    content_text text NOT NULL,
    delivery_kind text NOT NULL,
    expected_active_turn_id uuid,
    expected_defaults_version numeric(20, 0) NOT NULL,
    model_override_kind text NOT NULL,
    replacement_model_kind text,
    replacement_direct_model_selection_id uuid,
    replacement_model_alias_id uuid,
    acceptance_position numeric(20, 0) NOT NULL,
    disposition_kind text NOT NULL,
    origin_turn_id uuid NOT NULL UNIQUE,

    CONSTRAINT accepted_input_content_kind_closed
        CHECK (content_kind = 'text'),
    CONSTRAINT accepted_input_content_nonempty
        CHECK (char_length(content_text) > 0),
    CONSTRAINT accepted_input_delivery_shape
        CHECK (
            delivery_kind = 'start_when_no_active_turn'
            AND expected_active_turn_id IS NULL
        ),
    CONSTRAINT accepted_input_expected_defaults_positive_u64
        CHECK (
            expected_defaults_version >= 1
            AND expected_defaults_version <= 18446744073709551615
        ),
    CONSTRAINT accepted_input_configuration_shape
        CHECK (
            (
                model_override_kind = 'use_session_default'
                AND replacement_model_kind IS NULL
                AND replacement_direct_model_selection_id IS NULL
                AND replacement_model_alias_id IS NULL
            )
            OR
            (
                model_override_kind = 'replace_with'
                AND replacement_model_kind = 'direct'
                AND replacement_direct_model_selection_id IS NOT NULL
                AND replacement_model_alias_id IS NULL
            )
            OR
            (
                model_override_kind = 'replace_with'
                AND replacement_model_kind = 'alias'
                AND replacement_direct_model_selection_id IS NULL
                AND replacement_model_alias_id IS NOT NULL
            )
        ),
    CONSTRAINT accepted_input_position_positive_u64
        CHECK (
            acceptance_position >= 1
            AND acceptance_position <= 18446744073709551615
        ),
    CONSTRAINT accepted_input_disposition_shape
        CHECK (disposition_kind = 'origin_of'),
    CONSTRAINT accepted_input_session_position_key
        UNIQUE (session_id, acceptance_position),
    CONSTRAINT accepted_input_effect_key
        UNIQUE (
            accepted_input_id,
            session_id,
            acceptance_position,
            origin_turn_id
        ),
    CONSTRAINT accepted_input_result_key
        UNIQUE (accepted_input_id, session_id, origin_turn_id),
    CONSTRAINT accepted_input_session_fk
        FOREIGN KEY (session_id)
        REFERENCES session (session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CONSTRAINT accepted_input_command_result_fk
        FOREIGN KEY (
            accepting_command_id,
            accepted_input_id,
            session_id,
            origin_turn_id
        )
        REFERENCES submit_input_command (
            command_id,
            result_accepted_input_id,
            result_session_id,
            result_turn_id
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE TABLE queued_input_origin (
    turn_id uuid PRIMARY KEY,
    accepted_input_id uuid NOT NULL UNIQUE,
    session_id uuid NOT NULL,
    acceptance_position numeric(20, 0) NOT NULL,
    priority_kind text NOT NULL,
    defaults_version numeric(20, 0) NOT NULL,
    requested_model_kind text NOT NULL,
    requested_direct_model_selection_id uuid,
    requested_model_alias_id uuid,
    frozen_model_kind text NOT NULL,
    frozen_direct_model_selection_id uuid,
    frozen_model_alias_id uuid,
    frozen_alias_selected_direct_id uuid,
    model_parameters text NOT NULL,
    known_provider_failure_retry text NOT NULL,
    model_fallback text NOT NULL,

    CONSTRAINT queued_input_origin_position_positive_u64
        CHECK (
            acceptance_position >= 1
            AND acceptance_position <= 18446744073709551615
        ),
    CONSTRAINT queued_input_origin_priority_closed
        CHECK (priority_kind = 'ordinary'),
    CONSTRAINT queued_input_origin_defaults_version_positive_u64
        CHECK (
            defaults_version >= 1
            AND defaults_version <= 18446744073709551615
        ),
    CONSTRAINT queued_input_origin_requested_model_shape
        CHECK (
            (
                requested_model_kind = 'direct'
                AND requested_direct_model_selection_id IS NOT NULL
                AND requested_model_alias_id IS NULL
            )
            OR
            (
                requested_model_kind = 'alias'
                AND requested_direct_model_selection_id IS NULL
                AND requested_model_alias_id IS NOT NULL
            )
        ),
    CONSTRAINT queued_input_origin_frozen_model_shape
        CHECK (
            (
                frozen_model_kind = 'direct'
                AND frozen_direct_model_selection_id IS NOT NULL
                AND frozen_model_alias_id IS NULL
                AND frozen_alias_selected_direct_id IS NULL
            )
            OR
            (
                frozen_model_kind = 'frozen_alias'
                AND frozen_direct_model_selection_id IS NULL
                AND frozen_model_alias_id IS NOT NULL
                AND frozen_alias_selected_direct_id IS NOT NULL
            )
        ),
    CONSTRAINT queued_input_origin_model_parameters_closed
        CHECK (model_parameters = 'provider_defaults'),
    CONSTRAINT queued_input_origin_known_failure_retry_closed
        CHECK (known_provider_failure_retry = 'disabled'),
    CONSTRAINT queued_input_origin_model_fallback_closed
        CHECK (model_fallback = 'disabled'),
    CONSTRAINT queued_input_origin_effect_key
        UNIQUE (
            accepted_input_id,
            session_id,
            acceptance_position,
            turn_id
        ),
    CONSTRAINT queued_input_origin_defaults_fk
        FOREIGN KEY (session_id, defaults_version)
        REFERENCES session_defaults_version (session_id, version)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CONSTRAINT queued_input_origin_accepted_input_fk
        FOREIGN KEY (
            accepted_input_id,
            session_id,
            acceptance_position,
            turn_id
        )
        REFERENCES accepted_input (
            accepted_input_id,
            session_id,
            acceptance_position,
            origin_turn_id
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

ALTER TABLE accepted_input
    ADD CONSTRAINT accepted_input_queued_origin_fk
    FOREIGN KEY (
        accepted_input_id,
        session_id,
        acceptance_position,
        origin_turn_id
    )
    REFERENCES queued_input_origin (
        accepted_input_id,
        session_id,
        acceptance_position,
        turn_id
    )
    ON UPDATE RESTRICT
    ON DELETE RESTRICT
    DEFERRABLE INITIALLY DEFERRED;

ALTER TABLE submit_input_command
    ADD CONSTRAINT submit_input_command_applied_effect_fk
    FOREIGN KEY (
        result_accepted_input_id,
        result_session_id,
        result_turn_id
    )
    REFERENCES accepted_input (
        accepted_input_id,
        session_id,
        origin_turn_id
    )
    ON UPDATE RESTRICT
    ON DELETE RESTRICT
    DEFERRABLE INITIALLY DEFERRED;

CREATE FUNCTION require_submit_input_effect_correlation()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    matching_records bigint;
BEGIN
    IF NEW.result_kind = 'applied' THEN
        SELECT count(*)
          INTO matching_records
          FROM accepted_input AS accepted
          JOIN queued_input_origin AS queued
            ON queued.accepted_input_id = accepted.accepted_input_id
           AND queued.session_id = accepted.session_id
           AND queued.acceptance_position = accepted.acceptance_position
           AND queued.turn_id = accepted.origin_turn_id
          JOIN session_defaults_version AS defaults
            ON defaults.session_id = queued.session_id
           AND defaults.version = queued.defaults_version
         WHERE accepted.accepting_command_id = NEW.command_id
           AND accepted.accepted_input_id = NEW.result_accepted_input_id
           AND accepted.session_id = NEW.result_session_id
           AND accepted.content_kind = NEW.content_kind
           AND accepted.content_text = NEW.content_text
           AND accepted.delivery_kind = NEW.delivery_kind
           AND accepted.expected_active_turn_id IS NOT DISTINCT FROM NEW.expected_active_turn_id
           AND accepted.expected_defaults_version = NEW.expected_defaults_version
           AND accepted.model_override_kind = NEW.model_override_kind
           AND accepted.replacement_model_kind IS NOT DISTINCT FROM NEW.replacement_model_kind
           AND accepted.replacement_direct_model_selection_id
               IS NOT DISTINCT FROM NEW.replacement_direct_model_selection_id
           AND accepted.replacement_model_alias_id
               IS NOT DISTINCT FROM NEW.replacement_model_alias_id
           AND accepted.origin_turn_id = NEW.result_turn_id
           AND queued.priority_kind = 'ordinary'
           AND queued.defaults_version = NEW.expected_defaults_version
           AND (
               (
                   NEW.model_override_kind = 'use_session_default'
                   AND queued.requested_model_kind = defaults.model_selection_kind
                   AND queued.requested_direct_model_selection_id
                       IS NOT DISTINCT FROM defaults.direct_model_selection_id
                   AND queued.requested_model_alias_id
                       IS NOT DISTINCT FROM defaults.model_alias_id
               )
               OR
               (
                   NEW.model_override_kind = 'replace_with'
                   AND queued.requested_model_kind = NEW.replacement_model_kind
                   AND queued.requested_direct_model_selection_id
                       IS NOT DISTINCT FROM NEW.replacement_direct_model_selection_id
                   AND queued.requested_model_alias_id
                       IS NOT DISTINCT FROM NEW.replacement_model_alias_id
               )
           )
           AND (
               (
                   queued.requested_model_kind = 'direct'
                   AND queued.frozen_model_kind = 'direct'
                   AND queued.frozen_direct_model_selection_id
                       = queued.requested_direct_model_selection_id
               )
               OR
               (
                   queued.requested_model_kind = 'alias'
                   AND queued.frozen_model_kind = 'frozen_alias'
                   AND queued.frozen_model_alias_id = queued.requested_model_alias_id
               )
           )
           AND queued.model_parameters = 'provider_defaults'
           AND queued.known_provider_failure_retry = 'disabled'
           AND queued.model_fallback = 'disabled';
    ELSE
        SELECT count(*)
          INTO matching_records
          FROM accepted_input
         WHERE accepting_command_id = NEW.command_id;

        IF matching_records = 0
           AND NEW.rejection_kind = 'unknown_model_alias'
        THEN
            SELECT count(*)
              INTO matching_records
              FROM session_defaults_version AS defaults
             WHERE defaults.session_id = NEW.result_session_id
               AND defaults.version = NEW.result_selected_defaults_version
               AND (
                   (
                       NEW.model_override_kind = 'use_session_default'
                       AND defaults.model_selection_kind = 'alias'
                       AND defaults.model_alias_id = NEW.result_unknown_alias_id
                   )
                   OR
                   (
                       NEW.model_override_kind = 'replace_with'
                       AND NEW.replacement_model_kind = 'alias'
                       AND NEW.replacement_model_alias_id = NEW.result_unknown_alias_id
                   )
               );

            IF matching_records <> 1 THEN
                RAISE EXCEPTION
                    'submit-input command % has cross-wired unknown-alias evidence',
                    NEW.command_id
                    USING ERRCODE = '23503';
            END IF;
            matching_records := 0;
        END IF;
    END IF;

    IF matching_records <> (
        CASE WHEN NEW.result_kind = 'applied' THEN 1 ELSE 0 END
    ) THEN
        RAISE EXCEPTION
            'submit-input command % has an incomplete or cross-wired terminal effect',
            NEW.command_id
            USING ERRCODE = '23503';
    END IF;

    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER submit_input_command_requires_correlated_effect
AFTER INSERT ON submit_input_command
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_submit_input_effect_correlation();

CREATE TRIGGER submit_input_command_is_append_only
BEFORE UPDATE OR DELETE ON submit_input_command
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER accepted_input_is_append_only
BEFORE UPDATE OR DELETE ON accepted_input
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER queued_input_origin_is_append_only
BEFORE UPDATE OR DELETE ON queued_input_origin
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

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
        WHEN 'replace_session_defaults' THEN
            SELECT count(*)
              INTO matching_records
              FROM replace_session_defaults_command
             WHERE command_id = NEW.command_id;
        WHEN 'submit_input' THEN
            SELECT count(*)
              INTO matching_records
              FROM submit_input_command
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
