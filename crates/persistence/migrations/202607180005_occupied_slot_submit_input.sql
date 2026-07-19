-- Occupied-slot SubmitInput storage.
--
-- Existing version-one records remain valid. This migration opens only the
-- result and accepted-input shapes already admitted by the occupied-slot
-- domain slice: after-current turn origins, pending safe-point steering, and
-- the three active-slot rejections. Interrupt application remains absent.

ALTER TABLE submit_input_command
    ADD COLUMN result_actual_active_turn_id uuid;

ALTER TABLE accepted_input
    ALTER COLUMN expected_defaults_version DROP NOT NULL,
    ALTER COLUMN model_override_kind DROP NOT NULL,
    ALTER COLUMN origin_turn_id DROP NOT NULL;

ALTER TABLE submit_input_command
    DROP CONSTRAINT submit_input_command_rejection_kind_closed,
    DROP CONSTRAINT submit_input_command_result_shape;

ALTER TABLE submit_input_command
    ADD CONSTRAINT submit_input_command_rejection_kind_closed
    CHECK (
        rejection_kind IS NULL
        OR rejection_kind IN (
            'session_not_found',
            'no_active_turn',
            'active_turn_present',
            'active_turn_mismatch',
            'safe_point_unavailable_while_stopping',
            'session_defaults_version_mismatch',
            'unknown_model_alias',
            'acceptance_position_exhausted'
        )
    ),
    ADD CONSTRAINT submit_input_command_result_shape
    CHECK (
        (
            result_kind = 'applied'
            AND rejection_kind IS NULL
            AND delivery_kind IN (
                'start_when_no_active_turn',
                'after_current_turn'
            )
            AND result_accepted_input_id IS NOT NULL
            AND result_turn_id IS NOT NULL
            AND result_actual_active_turn_id IS NULL
            AND result_expected_active_turn_id IS NULL
            AND result_expected_defaults_version IS NULL
            AND result_current_defaults_version IS NULL
            AND result_unknown_alias_id IS NULL
            AND result_selected_defaults_version IS NULL
            AND result_last_position IS NULL
        )
        OR
        (
            result_kind = 'applied'
            AND rejection_kind IS NULL
            AND delivery_kind = 'next_safe_point'
            AND result_accepted_input_id IS NOT NULL
            AND result_turn_id IS NULL
            AND result_actual_active_turn_id IS NOT NULL
            AND result_actual_active_turn_id = expected_active_turn_id
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
            AND result_actual_active_turn_id IS NULL
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
            AND delivery_kind IN (
                'interrupt',
                'next_safe_point',
                'after_current_turn'
            )
            AND result_accepted_input_id IS NULL
            AND result_turn_id IS NULL
            AND result_actual_active_turn_id IS NULL
            AND result_expected_active_turn_id IS NOT NULL
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
            AND rejection_kind = 'active_turn_present'
            AND delivery_kind = 'start_when_no_active_turn'
            AND result_accepted_input_id IS NULL
            AND result_turn_id IS NULL
            AND result_actual_active_turn_id IS NOT NULL
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
            AND rejection_kind = 'active_turn_mismatch'
            AND delivery_kind IN (
                'interrupt',
                'next_safe_point',
                'after_current_turn'
            )
            AND result_accepted_input_id IS NULL
            AND result_turn_id IS NULL
            AND result_actual_active_turn_id IS NOT NULL
            AND result_expected_active_turn_id IS NOT NULL
            AND result_expected_active_turn_id = expected_active_turn_id
            AND result_actual_active_turn_id <> result_expected_active_turn_id
            AND result_expected_defaults_version IS NULL
            AND result_current_defaults_version IS NULL
            AND result_unknown_alias_id IS NULL
            AND result_selected_defaults_version IS NULL
            AND result_last_position IS NULL
        )
        OR
        (
            result_kind = 'rejected'
            AND rejection_kind = 'safe_point_unavailable_while_stopping'
            AND delivery_kind = 'next_safe_point'
            AND result_accepted_input_id IS NULL
            AND result_turn_id IS NULL
            AND result_actual_active_turn_id IS NOT NULL
            AND result_actual_active_turn_id = expected_active_turn_id
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
            AND rejection_kind = 'session_defaults_version_mismatch'
            AND delivery_kind IN (
                'start_when_no_active_turn',
                'after_current_turn'
            )
            AND result_accepted_input_id IS NULL
            AND result_turn_id IS NULL
            AND result_actual_active_turn_id IS NULL
            AND result_expected_active_turn_id IS NULL
            AND result_expected_defaults_version IS NOT NULL
            AND result_expected_defaults_version = expected_defaults_version
            AND result_current_defaults_version IS NOT NULL
            AND result_current_defaults_version
                <> result_expected_defaults_version
            AND result_unknown_alias_id IS NULL
            AND result_selected_defaults_version IS NULL
            AND result_last_position IS NULL
        )
        OR
        (
            result_kind = 'rejected'
            AND rejection_kind = 'unknown_model_alias'
            AND delivery_kind IN (
                'start_when_no_active_turn',
                'after_current_turn'
            )
            AND result_accepted_input_id IS NULL
            AND result_turn_id IS NULL
            AND result_actual_active_turn_id IS NULL
            AND result_expected_active_turn_id IS NULL
            AND result_expected_defaults_version IS NULL
            AND result_current_defaults_version IS NULL
            AND result_unknown_alias_id IS NOT NULL
            AND result_selected_defaults_version IS NOT NULL
            AND result_selected_defaults_version = expected_defaults_version
            AND result_last_position IS NULL
        )
        OR
        (
            result_kind = 'rejected'
            AND rejection_kind = 'acceptance_position_exhausted'
            AND delivery_kind IN (
                'start_when_no_active_turn',
                'next_safe_point',
                'after_current_turn'
            )
            AND result_accepted_input_id IS NULL
            AND result_turn_id IS NULL
            AND result_actual_active_turn_id IS NULL
            AND result_expected_active_turn_id IS NULL
            AND result_expected_defaults_version IS NULL
            AND result_current_defaults_version IS NULL
            AND result_unknown_alias_id IS NULL
            AND result_selected_defaults_version IS NULL
            AND result_last_position IS NOT NULL
            AND result_last_position = 18446744073709551615
        )
    );

ALTER TABLE accepted_input
    DROP CONSTRAINT accepted_input_delivery_shape,
    DROP CONSTRAINT accepted_input_disposition_shape;

ALTER TABLE accepted_input
    ADD CONSTRAINT accepted_input_delivery_shape
    CHECK (
        (
            disposition_kind = 'origin_of'
            AND delivery_kind IN (
                'start_when_no_active_turn',
                'after_current_turn'
            )
            AND (
                (
                    delivery_kind = 'start_when_no_active_turn'
                    AND expected_active_turn_id IS NULL
                )
                OR
                (
                    delivery_kind = 'after_current_turn'
                    AND expected_active_turn_id IS NOT NULL
                )
            )
            AND expected_defaults_version IS NOT NULL
            AND model_override_kind IS NOT NULL
            AND origin_turn_id IS NOT NULL
        )
        OR
        (
            disposition_kind = 'pending_steering'
            AND delivery_kind = 'next_safe_point'
            AND expected_active_turn_id IS NOT NULL
            AND expected_defaults_version IS NULL
            AND model_override_kind IS NULL
            AND replacement_model_kind IS NULL
            AND replacement_direct_model_selection_id IS NULL
            AND replacement_model_alias_id IS NULL
            AND origin_turn_id IS NULL
        )
    ),
    ADD CONSTRAINT accepted_input_disposition_closed
    CHECK (disposition_kind IN ('origin_of', 'pending_steering')),
    ADD CONSTRAINT accepted_input_pending_result_key
    UNIQUE (accepted_input_id, session_id, expected_active_turn_id),
    ADD CONSTRAINT accepted_input_expected_active_turn_fk
    FOREIGN KEY (expected_active_turn_id, session_id)
    REFERENCES turn_lifecycle (turn_id, session_id)
    ON UPDATE RESTRICT
    ON DELETE RESTRICT
    DEFERRABLE INITIALLY DEFERRED;

ALTER TABLE submit_input_command
    ADD CONSTRAINT submit_input_command_pending_correlation_key
    UNIQUE (
        command_id,
        result_accepted_input_id,
        result_session_id,
        result_actual_active_turn_id
    ),
    ADD CONSTRAINT submit_input_command_actual_active_turn_fk
    FOREIGN KEY (result_actual_active_turn_id, result_session_id)
    REFERENCES turn_lifecycle (turn_id, session_id)
    ON UPDATE RESTRICT
    ON DELETE RESTRICT
    DEFERRABLE INITIALLY DEFERRED,
    ADD CONSTRAINT submit_input_command_pending_effect_fk
    FOREIGN KEY (
        result_accepted_input_id,
        result_session_id,
        result_actual_active_turn_id
    )
    REFERENCES accepted_input (
        accepted_input_id,
        session_id,
        expected_active_turn_id
    )
    ON UPDATE RESTRICT
    ON DELETE RESTRICT
    DEFERRABLE INITIALLY DEFERRED;

ALTER TABLE accepted_input
    ADD CONSTRAINT accepted_input_general_command_result_key
    UNIQUE (accepting_command_id, accepted_input_id, session_id);

ALTER TABLE submit_input_command
    ADD CONSTRAINT submit_input_command_general_applied_key
    UNIQUE (command_id, result_accepted_input_id, result_session_id),
    ADD CONSTRAINT submit_input_command_general_applied_effect_fk
    FOREIGN KEY (command_id, result_accepted_input_id, result_session_id)
    REFERENCES accepted_input (
        accepting_command_id,
        accepted_input_id,
        session_id
    )
    ON UPDATE RESTRICT
    ON DELETE RESTRICT
    DEFERRABLE INITIALLY DEFERRED;

ALTER TABLE accepted_input
    ADD CONSTRAINT accepted_input_general_command_result_fk
    FOREIGN KEY (
        accepting_command_id,
        accepted_input_id,
        session_id
    )
    REFERENCES submit_input_command (
        command_id,
        result_accepted_input_id,
        result_session_id
    )
    ON UPDATE RESTRICT
    ON DELETE RESTRICT
    DEFERRABLE INITIALLY DEFERRED;

CREATE OR REPLACE FUNCTION require_submit_input_effect_correlation()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    matching_records bigint;
BEGIN
    IF NEW.result_kind = 'applied' AND NEW.result_turn_id IS NOT NULL THEN
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
           AND accepted.expected_active_turn_id
               IS NOT DISTINCT FROM NEW.expected_active_turn_id
           AND accepted.expected_defaults_version
               = NEW.expected_defaults_version
           AND accepted.model_override_kind = NEW.model_override_kind
           AND accepted.replacement_model_kind
               IS NOT DISTINCT FROM NEW.replacement_model_kind
           AND accepted.replacement_direct_model_selection_id
               IS NOT DISTINCT FROM NEW.replacement_direct_model_selection_id
           AND accepted.replacement_model_alias_id
               IS NOT DISTINCT FROM NEW.replacement_model_alias_id
           AND accepted.disposition_kind = 'origin_of'
           AND accepted.origin_turn_id = NEW.result_turn_id
           AND queued.priority_kind = 'ordinary'
           AND queued.defaults_version = NEW.expected_defaults_version
           AND (
               (
                   NEW.model_override_kind = 'use_session_default'
                   AND queued.requested_model_kind
                       = defaults.model_selection_kind
                   AND queued.requested_direct_model_selection_id
                       IS NOT DISTINCT FROM defaults.direct_model_selection_id
                   AND queued.requested_model_alias_id
                       IS NOT DISTINCT FROM defaults.model_alias_id
               )
               OR
               (
                   NEW.model_override_kind = 'replace_with'
                   AND queued.requested_model_kind
                       = NEW.replacement_model_kind
                   AND queued.requested_direct_model_selection_id
                       IS NOT DISTINCT FROM
                           NEW.replacement_direct_model_selection_id
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
                   AND queued.frozen_model_alias_id
                       = queued.requested_model_alias_id
               )
           )
           AND queued.model_parameters = 'provider_defaults'
           AND queued.known_provider_failure_retry = 'disabled'
           AND queued.model_fallback = 'disabled';
    ELSIF NEW.result_kind = 'applied' THEN
        SELECT count(*)
          INTO matching_records
          FROM accepted_input AS accepted
         WHERE accepted.accepting_command_id = NEW.command_id
           AND accepted.accepted_input_id = NEW.result_accepted_input_id
           AND accepted.session_id = NEW.result_session_id
           AND accepted.content_kind = NEW.content_kind
           AND accepted.content_text = NEW.content_text
           AND accepted.delivery_kind = 'next_safe_point'
           AND accepted.delivery_kind = NEW.delivery_kind
           AND accepted.expected_active_turn_id = NEW.expected_active_turn_id
           AND accepted.expected_defaults_version IS NULL
           AND accepted.model_override_kind IS NULL
           AND accepted.replacement_model_kind IS NULL
           AND accepted.replacement_direct_model_selection_id IS NULL
           AND accepted.replacement_model_alias_id IS NULL
           AND accepted.disposition_kind = 'pending_steering'
           AND accepted.origin_turn_id IS NULL
           AND accepted.expected_active_turn_id
               = NEW.result_actual_active_turn_id
           AND NOT EXISTS (
               SELECT 1
                 FROM queued_input_origin
                WHERE accepted_input_id = accepted.accepted_input_id
           );
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
                       AND NEW.replacement_model_alias_id
                           = NEW.result_unknown_alias_id
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
