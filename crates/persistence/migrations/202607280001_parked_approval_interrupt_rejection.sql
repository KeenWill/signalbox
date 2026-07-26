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

-- The row shape alone proves only that the receipt names the turn the command
-- expected. The sibling stopping rejections additionally prove the active
-- phase through `submit_input_command_requires_interrupt_effect`, which
-- correlates the named turn's stored `active_phase_kind` with the prior
-- applied interrupt's stopped attempt. This kind names no prior command, so it
-- would otherwise fall through to the legacy rejection trigger, whose only
-- rejection check is that the command accepted no input, so a receipt naming
-- a running or terminal turn would commit and replay as authoritative. The
-- same deferred trigger therefore proves the phase directly: a parked-approval
-- rejection requires its named turn to be active on an approval wait at
-- commit. The rejecting transaction never changes that phase, so the deferred
-- check observes exactly the phase the domain guard rejected against.

CREATE OR REPLACE FUNCTION require_interrupt_submit_input_effect_correlation()
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
          JOIN queued_input_origin AS successor
            ON successor.accepted_input_id = accepted.accepted_input_id
           AND successor.turn_id = accepted.origin_turn_id
           AND successor.session_id = accepted.session_id
           AND successor.acceptance_position = accepted.acceptance_position
          JOIN turn_attempt AS stopped_attempt
            ON stopped_attempt.turn_id = NEW.expected_active_turn_id
           AND stopped_attempt.session_id = NEW.session_id
           AND (
                (
                    stopped_attempt.interrupt_command_id = NEW.command_id
                    AND stopped_attempt.interrupt_predecessor_turn_id
                        = NEW.expected_active_turn_id
                    AND (
                        stopped_attempt.state_kind = 'stop_requested'
                        OR (
                            stopped_attempt.state_kind = 'ended'
                            AND stopped_attempt.end_variant = 'after_cancellation'
                        )
                    )
                )
                OR (
                    stopped_attempt.state_kind = 'ended'
                    AND stopped_attempt.end_variant = 'without_stop'
                    AND stopped_attempt.end_disposition IN ('ambiguous', 'lost')
                    AND stopped_attempt.interrupt_command_id IS NULL
                    AND stopped_attempt.interrupt_predecessor_turn_id IS NULL
                    AND EXISTS (
                        SELECT 1
                          FROM turn_lifecycle AS reconciled
                         WHERE reconciled.turn_id = stopped_attempt.turn_id
                           AND reconciled.session_id = stopped_attempt.session_id
                           AND reconciled.state_kind = 'terminal'
                           AND reconciled.terminal_disposition_kind
                               = 'reconciliation_required'
                           AND reconciled.terminal_attempt_id
                               = stopped_attempt.turn_attempt_id
                    )
                )
           )
         WHERE accepted.accepting_command_id = NEW.command_id
           AND accepted.accepted_input_id = NEW.result_accepted_input_id
           AND accepted.session_id = NEW.result_session_id
           AND accepted.content_kind = NEW.content_kind
           AND accepted.content_text = NEW.content_text
           AND accepted.delivery_kind = 'interrupt'
           AND accepted.expected_active_turn_id = NEW.expected_active_turn_id
           AND accepted.expected_defaults_version = NEW.expected_defaults_version
           AND accepted.model_override_kind = NEW.model_override_kind
           AND accepted.replacement_model_kind
               IS NOT DISTINCT FROM NEW.replacement_model_kind
           AND accepted.replacement_direct_model_selection_id
               IS NOT DISTINCT FROM NEW.replacement_direct_model_selection_id
           AND accepted.replacement_model_alias_id
               IS NOT DISTINCT FROM NEW.replacement_model_alias_id
           AND accepted.disposition_kind = 'origin_of'
           AND accepted.origin_turn_id = NEW.result_turn_id
           AND successor.priority_kind = 'interrupt_immediately_after'
           AND successor.interrupt_predecessor_turn_id
               = NEW.expected_active_turn_id
           AND successor.defaults_version = NEW.expected_defaults_version;
    ELSIF NEW.rejection_kind
        = 'interrupt_unavailable_while_awaiting_approval'
    THEN
        SELECT count(*)
          INTO matching_records
          FROM turn_lifecycle AS parked
         WHERE parked.turn_id = NEW.result_actual_active_turn_id
           AND parked.session_id = NEW.result_session_id
           AND parked.state_kind = 'active'
           AND parked.active_phase_kind = 'awaiting_tool_approval'
           AND NOT EXISTS (
                SELECT 1
                  FROM accepted_input
                 WHERE accepting_command_id = NEW.command_id
           );
    ELSE
        SELECT count(*)
          INTO matching_records
          FROM submit_input_command AS existing
          JOIN accepted_input AS accepted
            ON accepted.accepting_command_id = existing.command_id
           AND accepted.accepted_input_id = existing.result_accepted_input_id
           AND accepted.session_id = existing.result_session_id
           AND accepted.origin_turn_id = existing.result_turn_id
          JOIN queued_input_origin AS successor
            ON successor.accepted_input_id = accepted.accepted_input_id
           AND successor.turn_id = accepted.origin_turn_id
           AND successor.session_id = accepted.session_id
           AND successor.priority_kind = 'interrupt_immediately_after'
           AND successor.interrupt_predecessor_turn_id
               = NEW.result_actual_active_turn_id
          JOIN turn_lifecycle AS active
            ON active.turn_id = NEW.result_actual_active_turn_id
           AND active.session_id = NEW.result_session_id
           AND active.state_kind = 'active'
          JOIN turn_attempt AS stopped_attempt
            ON stopped_attempt.turn_attempt_id = active.current_attempt_id
           AND stopped_attempt.turn_id = active.turn_id
           AND stopped_attempt.session_id = active.session_id
           AND stopped_attempt.interrupt_command_id = existing.command_id
           AND stopped_attempt.interrupt_predecessor_turn_id = active.turn_id
           AND (
                (
                    active.active_phase_kind = 'running'
                    AND stopped_attempt.state_kind = 'stop_requested'
                )
                OR (
                    active.active_phase_kind = 'awaiting_model_call_recovery'
                    AND stopped_attempt.state_kind = 'ended'
                    AND stopped_attempt.end_variant = 'after_cancellation'
                    AND stopped_attempt.end_disposition IN ('ambiguous', 'lost')
                )
           )
         WHERE existing.command_id = NEW.result_existing_interrupt_command_id
           AND existing.result_kind = 'applied'
           AND existing.rejection_kind IS NULL
           AND existing.delivery_kind = 'interrupt'
           AND existing.expected_active_turn_id
               = NEW.result_actual_active_turn_id
           AND NOT EXISTS (
                SELECT 1
                  FROM accepted_input
                 WHERE accepting_command_id = NEW.command_id
           );
    END IF;

    IF matching_records <> 1 THEN
        RAISE EXCEPTION
            'interrupt submit-input command % has an incomplete or cross-wired effect',
            NEW.command_id
            USING ERRCODE = '23503';
    END IF;
    RETURN NULL;
END;
$$;

DROP TRIGGER submit_input_command_requires_correlated_effect
    ON submit_input_command;

DROP TRIGGER submit_input_command_requires_interrupt_effect
    ON submit_input_command;

CREATE CONSTRAINT TRIGGER submit_input_command_requires_correlated_effect
AFTER INSERT ON submit_input_command
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
WHEN (
    NOT (
        (
            NEW.result_kind = 'applied'
            AND NEW.delivery_kind = 'interrupt'
        )
        OR COALESCE(
            NEW.rejection_kind IN (
                'safe_point_unavailable_while_stopping',
                'interrupt_already_applied',
                'interrupt_unavailable_while_awaiting_approval'
            ),
            false
        )
    )
)
EXECUTE FUNCTION require_submit_input_legacy_effect_correlation();

CREATE CONSTRAINT TRIGGER submit_input_command_requires_interrupt_effect
AFTER INSERT ON submit_input_command
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
WHEN (
    (
        NEW.result_kind = 'applied'
        AND NEW.delivery_kind = 'interrupt'
    )
    OR COALESCE(
        NEW.rejection_kind IN (
            'safe_point_unavailable_while_stopping',
            'interrupt_already_applied',
            'interrupt_unavailable_while_awaiting_approval'
        ),
        false
    )
)
EXECUTE FUNCTION require_interrupt_submit_input_effect_correlation();
