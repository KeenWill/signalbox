-- Typed rejection for an interrupt delivered against a parked approval wait.
--
-- An approval wait remains parked until its canonical decision command
-- resolves the approval obligation; an interrupt alone is not a denial and
-- does not bypass the decision command. The submit transaction records
-- 'interrupt_unavailable_while_awaiting_approval' as an authoritative typed
-- rejection naming the active turn, exactly like the sibling invalid-interrupt
-- rejections, instead of failing the whole transaction.

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
            'session_defaults_version_mismatch',
            'unknown_model_alias',
            'acceptance_position_exhausted',
            'safe_point_unavailable_while_stopping',
            'interrupt_already_applied',
            'interrupt_unavailable_while_awaiting_approval'
        )
    ),
    ADD CONSTRAINT submit_input_command_result_shape
    CHECK (
        (
            result_kind = 'applied'
            AND rejection_kind IS NULL
            AND delivery_kind IN (
                'start_when_no_active_turn',
                'after_current_turn',
                'interrupt'
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
            AND result_existing_interrupt_command_id IS NULL
        )
        OR
        (
            result_kind = 'applied'
            AND rejection_kind IS NULL
            AND delivery_kind = 'next_safe_point'
            AND result_accepted_input_id IS NOT NULL
            AND result_turn_id IS NULL
            AND result_actual_active_turn_id = expected_active_turn_id
            AND result_actual_active_turn_id IS NOT NULL
            AND result_expected_active_turn_id IS NULL
            AND result_expected_defaults_version IS NULL
            AND result_current_defaults_version IS NULL
            AND result_unknown_alias_id IS NULL
            AND result_selected_defaults_version IS NULL
            AND result_last_position IS NULL
            AND result_existing_interrupt_command_id IS NULL
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
            AND result_existing_interrupt_command_id IS NULL
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
            AND result_expected_active_turn_id = expected_active_turn_id
            AND result_expected_active_turn_id IS NOT NULL
            AND result_expected_defaults_version IS NULL
            AND result_current_defaults_version IS NULL
            AND result_unknown_alias_id IS NULL
            AND result_selected_defaults_version IS NULL
            AND result_last_position IS NULL
            AND result_existing_interrupt_command_id IS NULL
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
            AND result_existing_interrupt_command_id IS NULL
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
            AND result_expected_active_turn_id = expected_active_turn_id
            AND result_expected_active_turn_id IS NOT NULL
            AND result_actual_active_turn_id <> result_expected_active_turn_id
            AND result_expected_defaults_version IS NULL
            AND result_current_defaults_version IS NULL
            AND result_unknown_alias_id IS NULL
            AND result_selected_defaults_version IS NULL
            AND result_last_position IS NULL
            AND result_existing_interrupt_command_id IS NULL
        )
        OR
        (
            result_kind = 'rejected'
            AND rejection_kind = 'session_defaults_version_mismatch'
            AND delivery_kind IN (
                'start_when_no_active_turn',
                'after_current_turn',
                'interrupt'
            )
            AND result_accepted_input_id IS NULL
            AND result_turn_id IS NULL
            AND result_actual_active_turn_id IS NULL
            AND result_expected_active_turn_id IS NULL
            AND result_expected_defaults_version = expected_defaults_version
            AND result_expected_defaults_version IS NOT NULL
            AND result_current_defaults_version IS NOT NULL
            AND result_current_defaults_version <> result_expected_defaults_version
            AND result_unknown_alias_id IS NULL
            AND result_selected_defaults_version IS NULL
            AND result_last_position IS NULL
            AND result_existing_interrupt_command_id IS NULL
        )
        OR
        (
            result_kind = 'rejected'
            AND rejection_kind = 'unknown_model_alias'
            AND delivery_kind IN (
                'start_when_no_active_turn',
                'after_current_turn',
                'interrupt'
            )
            AND result_accepted_input_id IS NULL
            AND result_turn_id IS NULL
            AND result_actual_active_turn_id IS NULL
            AND result_expected_active_turn_id IS NULL
            AND result_expected_defaults_version IS NULL
            AND result_current_defaults_version IS NULL
            AND result_unknown_alias_id IS NOT NULL
            AND result_selected_defaults_version = expected_defaults_version
            AND result_selected_defaults_version IS NOT NULL
            AND result_last_position IS NULL
            AND result_existing_interrupt_command_id IS NULL
        )
        OR
        (
            result_kind = 'rejected'
            AND rejection_kind = 'acceptance_position_exhausted'
            AND delivery_kind IN (
                'start_when_no_active_turn',
                'next_safe_point',
                'after_current_turn',
                'interrupt'
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
            AND result_existing_interrupt_command_id IS NULL
        )
        OR
        (
            result_kind = 'rejected'
            AND (
                (
                    rejection_kind = 'safe_point_unavailable_while_stopping'
                    AND delivery_kind = 'next_safe_point'
                )
                OR (
                    rejection_kind = 'interrupt_already_applied'
                    AND delivery_kind = 'interrupt'
                )
            )
            AND result_accepted_input_id IS NULL
            AND result_turn_id IS NULL
            AND result_actual_active_turn_id = expected_active_turn_id
            AND result_actual_active_turn_id IS NOT NULL
            AND result_expected_active_turn_id IS NULL
            AND result_expected_defaults_version IS NULL
            AND result_current_defaults_version IS NULL
            AND result_unknown_alias_id IS NULL
            AND result_selected_defaults_version IS NULL
            AND result_last_position IS NULL
            AND result_existing_interrupt_command_id IS NOT NULL
        )
        OR
        (
            result_kind = 'rejected'
            AND rejection_kind = 'interrupt_unavailable_while_awaiting_approval'
            AND delivery_kind = 'interrupt'
            AND result_accepted_input_id IS NULL
            AND result_turn_id IS NULL
            AND result_actual_active_turn_id = expected_active_turn_id
            AND result_actual_active_turn_id IS NOT NULL
            AND result_expected_active_turn_id IS NULL
            AND result_expected_defaults_version IS NULL
            AND result_current_defaults_version IS NULL
            AND result_unknown_alias_id IS NULL
            AND result_selected_defaults_version IS NULL
            AND result_last_position IS NULL
            AND result_existing_interrupt_command_id IS NULL
        )
    );
