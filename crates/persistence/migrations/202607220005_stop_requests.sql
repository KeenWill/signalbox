-- Atomic interrupt acceptance, durable stop requests, and cancelled turns.
--
-- An applied Interrupt records both the immediate-successor queue edge and the
-- exact stop proof on the predecessor's attempt. Prepared work may cancel
-- directly; issued work first enters cancellation_requested. All terminal
-- races retain the same immutable interrupt proof.

ALTER TABLE submit_input_command
    ADD COLUMN result_existing_interrupt_command_id uuid,
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
            'interrupt_already_applied'
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
    ),
    ADD CONSTRAINT submit_input_command_existing_interrupt_fk
        FOREIGN KEY (result_existing_interrupt_command_id)
        REFERENCES submit_input_command (command_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED;

ALTER TABLE accepted_input
    DROP CONSTRAINT accepted_input_delivery_shape;

ALTER TABLE accepted_input
    ADD CONSTRAINT accepted_input_delivery_shape
    CHECK (
        (
            disposition_kind = 'origin_of'
            AND delivery_kind IN (
                'start_when_no_active_turn',
                'after_current_turn',
                'interrupt'
            )
            AND (
                (
                    delivery_kind = 'start_when_no_active_turn'
                    AND expected_active_turn_id IS NULL
                )
                OR
                (
                    delivery_kind IN ('after_current_turn', 'interrupt')
                    AND expected_active_turn_id IS NOT NULL
                )
            )
            AND expected_defaults_version IS NOT NULL
            AND model_override_kind IS NOT NULL
            AND origin_turn_id IS NOT NULL
            AND consuming_model_call_id IS NULL
        )
        OR
        (
            disposition_kind IN (
                'pending_steering',
                'consumed_as_steering'
            )
            AND delivery_kind = 'next_safe_point'
            AND expected_active_turn_id IS NOT NULL
            AND expected_defaults_version IS NULL
            AND model_override_kind IS NULL
            AND replacement_model_kind IS NULL
            AND replacement_direct_model_selection_id IS NULL
            AND replacement_model_alias_id IS NULL
            AND origin_turn_id IS NULL
            AND (
                (
                    disposition_kind = 'pending_steering'
                    AND consuming_model_call_id IS NULL
                )
                OR
                (
                    disposition_kind = 'consumed_as_steering'
                    AND consuming_model_call_id IS NOT NULL
                )
            )
        )
        OR
        (
            disposition_kind = 'reclassified_as_turn_origin'
            AND delivery_kind = 'next_safe_point'
            AND expected_active_turn_id IS NOT NULL
            AND expected_defaults_version IS NULL
            AND model_override_kind IS NULL
            AND replacement_model_kind IS NULL
            AND replacement_direct_model_selection_id IS NULL
            AND replacement_model_alias_id IS NULL
            AND origin_turn_id IS NOT NULL
            AND consuming_model_call_id IS NULL
        )
    );

ALTER TABLE queued_input_origin
    ADD COLUMN interrupt_predecessor_turn_id uuid,
    DROP CONSTRAINT queued_input_origin_priority_closed;

ALTER TABLE queued_input_origin
    ADD CONSTRAINT queued_input_origin_priority_closed
    CHECK (
        (
            priority_kind = 'ordinary'
            AND interrupt_predecessor_turn_id IS NULL
        )
        OR
        (
            priority_kind = 'interrupt_immediately_after'
            AND interrupt_predecessor_turn_id IS NOT NULL
        )
    ),
    ADD CONSTRAINT queued_input_origin_interrupt_predecessor_fk
        FOREIGN KEY (interrupt_predecessor_turn_id, session_id)
        REFERENCES turn_lifecycle (turn_id, session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    ADD CONSTRAINT queued_input_origin_interrupt_edge_once
        UNIQUE (interrupt_predecessor_turn_id);

CREATE FUNCTION accepted_input_turn_queue_predecessor(
    checked_session uuid,
    checked_turn uuid
)
RETURNS uuid
LANGUAGE sql
STABLE
AS $$
    WITH RECURSIVE derived_order (
        turn_id,
        root_position,
        interrupt_depth
    ) AS (
        SELECT
            lifecycle.turn_id,
            lifecycle.acceptance_position,
            0::bigint
          FROM turn_lifecycle AS lifecycle
          JOIN queued_input_origin AS origin
            ON origin.turn_id = lifecycle.turn_id
           AND origin.session_id = lifecycle.session_id
         WHERE lifecycle.session_id = checked_session
           AND origin.priority_kind = 'ordinary'
        UNION ALL
        SELECT
            successor.turn_id,
            predecessor.root_position,
            predecessor.interrupt_depth + 1
          FROM derived_order AS predecessor
          JOIN queued_input_origin AS successor
            ON successor.session_id = checked_session
           AND successor.priority_kind = 'interrupt_immediately_after'
           AND successor.interrupt_predecessor_turn_id = predecessor.turn_id
          JOIN turn_lifecycle AS successor_lifecycle
            ON successor_lifecycle.turn_id = successor.turn_id
           AND successor_lifecycle.session_id = successor.session_id
    ),
    ranked AS (
        SELECT
            turn_id,
            lag(turn_id) OVER (
                ORDER BY root_position, interrupt_depth
            ) AS predecessor_turn
          FROM derived_order
    )
    SELECT predecessor_turn
      FROM ranked
     WHERE turn_id = checked_turn;
$$;

CREATE FUNCTION accepted_input_turn_is_first_nonterminal(
    checked_session uuid,
    checked_turn uuid
)
RETURNS boolean
LANGUAGE sql
STABLE
AS $$
    WITH RECURSIVE derived_order (
        turn_id,
        root_position,
        interrupt_depth
    ) AS (
        SELECT
            lifecycle.turn_id,
            lifecycle.acceptance_position,
            0::bigint
          FROM turn_lifecycle AS lifecycle
          JOIN queued_input_origin AS origin
            ON origin.turn_id = lifecycle.turn_id
           AND origin.session_id = lifecycle.session_id
         WHERE lifecycle.session_id = checked_session
           AND origin.priority_kind = 'ordinary'
        UNION ALL
        SELECT
            successor.turn_id,
            predecessor.root_position,
            predecessor.interrupt_depth + 1
          FROM derived_order AS predecessor
          JOIN queued_input_origin AS successor
            ON successor.session_id = checked_session
           AND successor.priority_kind = 'interrupt_immediately_after'
           AND successor.interrupt_predecessor_turn_id = predecessor.turn_id
          JOIN turn_lifecycle AS successor_lifecycle
            ON successor_lifecycle.turn_id = successor.turn_id
           AND successor_lifecycle.session_id = successor.session_id
    ),
    ranked AS (
        SELECT
            turn_id,
            row_number() OVER (
                ORDER BY root_position, interrupt_depth
            ) AS queue_rank
          FROM derived_order
    ),
    candidate AS (
        SELECT queue_rank
          FROM ranked
         WHERE turn_id = checked_turn
    )
    SELECT EXISTS (SELECT 1 FROM candidate)
       AND NOT EXISTS (
            SELECT 1
              FROM ranked AS earlier
              JOIN turn_lifecycle AS lifecycle
                ON lifecycle.turn_id = earlier.turn_id
               AND lifecycle.session_id = checked_session
              JOIN candidate
                ON earlier.queue_rank < candidate.queue_rank
             WHERE lifecycle.state_kind <> 'terminal'
       );
$$;

ALTER TABLE turn_attempt
    ADD COLUMN interrupt_command_id uuid,
    ADD COLUMN interrupt_predecessor_turn_id uuid,
    DROP CONSTRAINT turn_attempt_state_kind_closed,
    DROP CONSTRAINT turn_attempt_end_variant_closed,
    DROP CONSTRAINT turn_attempt_end_disposition_closed,
    DROP CONSTRAINT turn_attempt_state_payload_shape;

ALTER TABLE turn_attempt
    ADD CONSTRAINT turn_attempt_state_kind_closed
        CHECK (
            state_kind IN (
                'prepared',
                'running',
                'stop_requested',
                'ended'
            )
        ),
    ADD CONSTRAINT turn_attempt_end_variant_closed
        CHECK (
            end_variant IS NULL
            OR end_variant IN ('without_stop', 'after_cancellation')
        ),
    ADD CONSTRAINT turn_attempt_end_disposition_closed
        CHECK (
            end_disposition IS NULL
            OR end_disposition IN (
                'turn_completed',
                'turn_refused',
                'yielded_to_durable_wait',
                'known_failure',
                'lost',
                'cancelled',
                'ambiguous'
            )
        ),
    ADD CONSTRAINT turn_attempt_state_payload_shape
        CHECK (
            (
                state_kind IN ('prepared', 'running')
                AND end_variant IS NULL
                AND end_disposition IS NULL
                AND interrupt_command_id IS NULL
                AND interrupt_predecessor_turn_id IS NULL
            )
            OR
            (
                state_kind = 'stop_requested'
                AND end_variant IS NULL
                AND end_disposition IS NULL
                AND interrupt_command_id IS NOT NULL
                AND interrupt_predecessor_turn_id = turn_id
            )
            OR
            (
                state_kind = 'ended'
                AND end_variant = 'without_stop'
                AND end_disposition IS NOT NULL
                AND interrupt_command_id IS NULL
                AND interrupt_predecessor_turn_id IS NULL
            )
            OR
            (
                state_kind = 'ended'
                AND end_variant = 'after_cancellation'
                AND end_disposition IN (
                    'turn_completed',
                    'turn_refused',
                    'known_failure',
                    'lost',
                    'cancelled',
                    'ambiguous'
                )
                AND interrupt_command_id IS NOT NULL
                AND interrupt_predecessor_turn_id = turn_id
            )
        ),
    ADD CONSTRAINT turn_attempt_interrupt_command_once
        UNIQUE (interrupt_command_id),
    ADD CONSTRAINT turn_attempt_interrupt_command_fk
        FOREIGN KEY (interrupt_command_id)
        REFERENCES submit_input_command (command_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    ADD CONSTRAINT turn_attempt_interrupt_predecessor_fk
        FOREIGN KEY (interrupt_predecessor_turn_id, session_id)
        REFERENCES turn_lifecycle (turn_id, session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED;

CREATE OR REPLACE FUNCTION reject_turn_attempt_invalid_change()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    owning_turn_state text;
BEGIN
    IF TG_OP = 'INSERT' THEN
        UPDATE turn_lifecycle
           SET attempt_history_present = true
         WHERE turn_id = NEW.turn_id
           AND session_id = NEW.session_id
           AND state_kind <> 'terminal'
        RETURNING state_kind
          INTO owning_turn_state;

        IF NOT FOUND THEN
            RAISE EXCEPTION 'a terminal turn cannot acquire another attempt'
                USING ERRCODE = '23514';
        END IF;

        IF NEW.state_kind <> 'prepared' THEN
            RAISE EXCEPTION 'turn attempt must be inserted as prepared'
                USING
                    ERRCODE = '23514',
                    CONSTRAINT = 'turn_attempt_inserted_prepared';
        END IF;
        RETURN NEW;
    END IF;

    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'turn_attempt is not deletable'
            USING ERRCODE = '23514';
    END IF;

    IF ROW(
        OLD.turn_attempt_id,
        OLD.turn_id,
        OLD.session_id,
        OLD.continued_from_attempt_id
    ) IS DISTINCT FROM ROW(
        NEW.turn_attempt_id,
        NEW.turn_id,
        NEW.session_id,
        NEW.continued_from_attempt_id
    ) THEN
        RAISE EXCEPTION 'turn attempt identity, ownership, and predecessor are immutable'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.state_kind = 'ended' THEN
        RAISE EXCEPTION 'ended turn attempt is immutable'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.interrupt_command_id IS NOT NULL
       AND ROW(
            OLD.interrupt_command_id,
            OLD.interrupt_predecessor_turn_id
       ) IS DISTINCT FROM ROW(
            NEW.interrupt_command_id,
            NEW.interrupt_predecessor_turn_id
       )
    THEN
        RAISE EXCEPTION 'turn attempt interrupt proof is immutable'
            USING ERRCODE = '23514';
    END IF;

    IF NOT (
        OLD.state_kind = NEW.state_kind
        OR (
            OLD.state_kind = 'prepared'
            AND NEW.state_kind IN ('running', 'ended')
        )
        OR (
            OLD.state_kind = 'running'
            AND NEW.state_kind IN ('stop_requested', 'ended')
        )
        OR (
            OLD.state_kind = 'stop_requested'
            AND NEW.state_kind = 'ended'
        )
    ) THEN
        RAISE EXCEPTION 'turn attempt transition is not monotonic'
            USING ERRCODE = '23514';
    END IF;

    RETURN NEW;
END;
$$;

ALTER TABLE turn_lifecycle
    DROP CONSTRAINT turn_lifecycle_terminal_disposition_closed,
    DROP CONSTRAINT turn_lifecycle_state_payload_shape;

ALTER TABLE turn_lifecycle
    ADD CONSTRAINT turn_lifecycle_terminal_disposition_closed
        CHECK (
            terminal_disposition_kind IS NULL
            OR terminal_disposition_kind IN (
                'failed',
                'completed',
                'refused',
                'cancelled',
                'reconciliation_required'
            )
        ),
    ADD CONSTRAINT turn_lifecycle_state_payload_shape
        CHECK (
            (
                state_kind = 'queued'
                AND start_lineage_kind IS NULL
                AND immediate_predecessor_turn_id IS NULL
                AND starting_frontier_id IS NULL
                AND terminal_frontier_id IS NULL
                AND active_phase_kind IS NULL
                AND current_attempt_id IS NULL
                AND terminal_disposition_kind IS NULL
                AND recovery_model_call_id IS NULL
                AND terminal_attempt_id IS NULL
                AND terminal_model_call_id IS NULL
            )
            OR
            (
                state_kind = 'active'
                AND start_lineage_kind IS NOT NULL
                AND starting_frontier_id IS NOT NULL
                AND terminal_frontier_id IS NULL
                AND active_phase_kind = 'running'
                AND current_attempt_id IS NOT NULL
                AND terminal_disposition_kind IS NULL
                AND recovery_model_call_id IS NULL
                AND terminal_attempt_id IS NULL
                AND terminal_model_call_id IS NULL
            )
            OR
            (
                state_kind = 'active'
                AND start_lineage_kind IS NOT NULL
                AND starting_frontier_id IS NOT NULL
                AND terminal_frontier_id IS NULL
                AND active_phase_kind = 'awaiting_model_call_recovery'
                AND current_attempt_id IS NOT NULL
                AND terminal_disposition_kind IS NULL
                AND recovery_model_call_id IS NOT NULL
                AND terminal_attempt_id IS NULL
                AND terminal_model_call_id IS NULL
            )
            OR
            (
                state_kind = 'terminal'
                AND start_lineage_kind IS NOT NULL
                AND starting_frontier_id IS NOT NULL
                AND terminal_frontier_id IS NOT NULL
                AND active_phase_kind IS NULL
                AND current_attempt_id IS NULL
                AND terminal_disposition_kind = 'failed'
                AND recovery_model_call_id IS NULL
                AND (
                    (
                        terminal_attempt_id IS NULL
                        AND terminal_model_call_id IS NULL
                    )
                    OR terminal_attempt_id IS NOT NULL
                )
            )
            OR
            (
                state_kind = 'terminal'
                AND start_lineage_kind IS NOT NULL
                AND starting_frontier_id IS NOT NULL
                AND terminal_frontier_id IS NOT NULL
                AND active_phase_kind IS NULL
                AND current_attempt_id IS NULL
                AND terminal_disposition_kind IN ('completed', 'refused')
                AND recovery_model_call_id IS NULL
                AND terminal_attempt_id IS NOT NULL
                AND terminal_model_call_id IS NOT NULL
            )
            OR
            (
                state_kind = 'terminal'
                AND start_lineage_kind IS NOT NULL
                AND starting_frontier_id IS NOT NULL
                AND terminal_frontier_id IS NOT NULL
                AND active_phase_kind IS NULL
                AND current_attempt_id IS NULL
                AND terminal_disposition_kind = 'cancelled'
                AND recovery_model_call_id IS NULL
                AND terminal_attempt_id IS NOT NULL
            )
            OR
            (
                state_kind = 'terminal'
                AND start_lineage_kind IS NOT NULL
                AND starting_frontier_id IS NOT NULL
                AND terminal_frontier_id IS NOT NULL
                AND active_phase_kind IS NULL
                AND current_attempt_id IS NULL
                AND terminal_disposition_kind = 'reconciliation_required'
                AND recovery_model_call_id IS NULL
                AND terminal_attempt_id IS NOT NULL
                AND terminal_model_call_id IS NOT NULL
            )
        );

ALTER TABLE semantic_transcript_entry
    ADD COLUMN cancelled_turn_id uuid,
    DROP CONSTRAINT semantic_transcript_entry_payload_kind_closed,
    DROP CONSTRAINT semantic_transcript_entry_payload_shape;

ALTER TABLE semantic_transcript_entry
    ADD CONSTRAINT semantic_transcript_entry_payload_kind_closed
        CHECK (
            payload_kind IN (
                'origin_accepted_input',
                'steering_accepted_input',
                'turn_failed',
                'assistant_text',
                'assistant_tool_use',
                'turn_completed',
                'turn_cancelled'
            )
        ),
    ADD CONSTRAINT semantic_transcript_entry_payload_shape
        CHECK (
            (
                payload_kind IN (
                    'origin_accepted_input',
                    'steering_accepted_input'
                )
                AND origin_accepted_input_id IS NOT NULL
                AND (
                    (
                        payload_kind = 'origin_accepted_input'
                        AND steering_source_turn_id IS NULL
                    )
                    OR (
                        payload_kind = 'steering_accepted_input'
                        AND steering_source_turn_id IS NOT NULL
                    )
                )
                AND failed_turn_id IS NULL
                AND assistant_text_value IS NULL
                AND producing_model_call_id IS NULL
                AND assistant_tool_request_id IS NULL
                AND completed_turn_id IS NULL
                AND cancelled_turn_id IS NULL
            )
            OR
            (
                payload_kind = 'turn_failed'
                AND origin_accepted_input_id IS NULL
                AND steering_source_turn_id IS NULL
                AND failed_turn_id IS NOT NULL
                AND assistant_text_value IS NULL
                AND producing_model_call_id IS NULL
                AND assistant_tool_request_id IS NULL
                AND completed_turn_id IS NULL
                AND cancelled_turn_id IS NULL
            )
            OR
            (
                payload_kind = 'assistant_text'
                AND origin_accepted_input_id IS NULL
                AND steering_source_turn_id IS NULL
                AND failed_turn_id IS NULL
                AND assistant_text_value IS NOT NULL
                AND producing_model_call_id IS NOT NULL
                AND assistant_tool_request_id IS NULL
                AND completed_turn_id IS NULL
                AND cancelled_turn_id IS NULL
                AND assistant_text_value <> ''
            )
            OR
            (
                payload_kind = 'assistant_tool_use'
                AND origin_accepted_input_id IS NULL
                AND steering_source_turn_id IS NULL
                AND failed_turn_id IS NULL
                AND assistant_text_value IS NULL
                AND producing_model_call_id IS NOT NULL
                AND assistant_tool_request_id IS NOT NULL
                AND completed_turn_id IS NULL
                AND cancelled_turn_id IS NULL
            )
            OR
            (
                payload_kind = 'turn_completed'
                AND origin_accepted_input_id IS NULL
                AND steering_source_turn_id IS NULL
                AND failed_turn_id IS NULL
                AND assistant_text_value IS NULL
                AND producing_model_call_id IS NULL
                AND assistant_tool_request_id IS NULL
                AND completed_turn_id IS NOT NULL
                AND cancelled_turn_id IS NULL
            )
            OR
            (
                payload_kind = 'turn_cancelled'
                AND origin_accepted_input_id IS NULL
                AND steering_source_turn_id IS NULL
                AND failed_turn_id IS NULL
                AND assistant_text_value IS NULL
                AND producing_model_call_id IS NULL
                AND assistant_tool_request_id IS NULL
                AND completed_turn_id IS NULL
                AND cancelled_turn_id IS NOT NULL
            )
        ),
    ADD CONSTRAINT semantic_transcript_entry_turn_cancelled_once
        UNIQUE (cancelled_turn_id),
    ADD CONSTRAINT semantic_transcript_entry_cancelled_turn_fk
        FOREIGN KEY (cancelled_turn_id, source_session_id)
        REFERENCES turn_lifecycle (turn_id, session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED;

ALTER TABLE model_call_transition_outbox_event
    DROP CONSTRAINT model_call_transition_outbox_state_closed;

ALTER TABLE model_call_transition_outbox_event
    ADD CONSTRAINT model_call_transition_outbox_state_closed
        CHECK (
            call_state_kind IN (
                'prepared',
                'in_flight',
                'cancellation_requested',
                'terminal'
            )
        );

ALTER TABLE outbox_event
    DROP CONSTRAINT outbox_event_kind_closed;

ALTER TABLE outbox_event
    ADD CONSTRAINT outbox_event_kind_closed
        CHECK (
            event_kind IN (
                'session_created',
                'turn_failed',
                'model_call_transition',
                'turn_completed',
                'turn_refused',
                'turn_cancelled',
                'turn_reconciliation_required'
            )
        );

CREATE TABLE turn_cancelled_outbox_event (
    event_sequence numeric(20, 0) PRIMARY KEY,
    event_kind text NOT NULL,
    storage_version smallint NOT NULL,
    session_id uuid NOT NULL,
    turn_id uuid NOT NULL UNIQUE,
    cancellation_entry_id uuid NOT NULL UNIQUE,
    terminal_frontier_id uuid NOT NULL UNIQUE,

    CONSTRAINT turn_cancelled_outbox_kind_closed
        CHECK (event_kind = 'turn_cancelled'),
    CONSTRAINT turn_cancelled_outbox_version_supported
        CHECK (storage_version = 1),
    CONSTRAINT turn_cancelled_outbox_header_fk
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
    CONSTRAINT turn_cancelled_outbox_turn_fk
        FOREIGN KEY (turn_id, session_id)
        REFERENCES turn_lifecycle (turn_id, session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT turn_cancelled_outbox_entry_fk
        FOREIGN KEY (session_id, cancellation_entry_id)
        REFERENCES semantic_transcript_entry (
            source_session_id,
            semantic_entry_id
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT turn_cancelled_outbox_frontier_fk
        FOREIGN KEY (session_id, terminal_frontier_id)
        REFERENCES context_frontier (owning_session_id, context_frontier_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE TRIGGER turn_cancelled_outbox_event_is_append_only
BEFORE UPDATE OR DELETE ON turn_cancelled_outbox_event
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER turn_cancelled_outbox_event_cannot_be_truncated
BEFORE TRUNCATE ON turn_cancelled_outbox_event
FOR EACH STATEMENT
EXECUTE FUNCTION reject_outbox_table_truncate();

CREATE TABLE turn_reconciliation_required_outbox_event (
    event_sequence numeric(20, 0) PRIMARY KEY,
    event_kind text NOT NULL,
    storage_version smallint NOT NULL,
    session_id uuid NOT NULL,
    turn_id uuid NOT NULL UNIQUE,
    model_call_id uuid NOT NULL UNIQUE,
    terminal_frontier_id uuid NOT NULL UNIQUE,

    CONSTRAINT turn_reconciliation_required_outbox_kind_closed
        CHECK (event_kind = 'turn_reconciliation_required'),
    CONSTRAINT turn_reconciliation_required_outbox_version_supported
        CHECK (storage_version = 1),
    CONSTRAINT turn_reconciliation_required_outbox_header_fk
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
    CONSTRAINT turn_reconciliation_required_outbox_turn_fk
        FOREIGN KEY (turn_id, session_id)
        REFERENCES turn_lifecycle (turn_id, session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT turn_reconciliation_required_outbox_call_fk
        FOREIGN KEY (model_call_id, turn_id, session_id)
        REFERENCES model_call (model_call_id, turn_id, session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT turn_reconciliation_required_outbox_frontier_fk
        FOREIGN KEY (session_id, terminal_frontier_id)
        REFERENCES context_frontier (owning_session_id, context_frontier_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE TRIGGER turn_reconciliation_required_outbox_event_is_append_only
BEFORE UPDATE OR DELETE ON turn_reconciliation_required_outbox_event
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER turn_reconciliation_required_outbox_event_cannot_be_truncated
BEFORE TRUNCATE ON turn_reconciliation_required_outbox_event
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
        WHEN 'turn_failed' THEN
            SELECT count(*) INTO matching_records
              FROM turn_failed_outbox_event
             WHERE event_sequence = NEW.event_sequence;
        WHEN 'model_call_transition' THEN
            SELECT count(*) INTO matching_records
              FROM model_call_transition_outbox_event
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

CREATE FUNCTION assert_interrupt_attempt_proof(
    checked_attempt_id uuid
)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    checked_turn uuid;
    checked_session uuid;
    checked_command uuid;
    matching_records bigint;
BEGIN
    SELECT
        turn_id,
        session_id,
        interrupt_command_id
      INTO
        checked_turn,
        checked_session,
        checked_command
      FROM turn_attempt
     WHERE turn_attempt_id = checked_attempt_id
       AND interrupt_command_id IS NOT NULL;

    IF NOT FOUND THEN
        RETURN;
    END IF;

    SELECT count(*)
      INTO matching_records
      FROM submit_input_command AS command
      JOIN accepted_input AS accepted
        ON accepted.accepting_command_id = command.command_id
       AND accepted.accepted_input_id = command.result_accepted_input_id
       AND accepted.session_id = command.result_session_id
       AND accepted.origin_turn_id = command.result_turn_id
       AND accepted.delivery_kind = 'interrupt'
       AND accepted.expected_active_turn_id = checked_turn
       AND accepted.disposition_kind = 'origin_of'
      JOIN queued_input_origin AS successor
        ON successor.accepted_input_id = accepted.accepted_input_id
       AND successor.turn_id = accepted.origin_turn_id
       AND successor.session_id = accepted.session_id
       AND successor.priority_kind = 'interrupt_immediately_after'
       AND successor.interrupt_predecessor_turn_id = checked_turn
     WHERE command.command_id = checked_command
       AND command.result_kind = 'applied'
       AND command.rejection_kind IS NULL
       AND command.delivery_kind = 'interrupt'
       AND command.session_id = checked_session
       AND command.expected_active_turn_id = checked_turn;

    IF matching_records <> 1 THEN
        RAISE EXCEPTION 'turn attempt % lacks its exact applied interrupt proof',
            checked_attempt_id
            USING
                ERRCODE = '23503',
                CONSTRAINT = 'turn_attempt_interrupt_proof';
    END IF;
END;
$$;

CREATE FUNCTION require_interrupt_attempt_proof()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    PERFORM assert_interrupt_attempt_proof(
        CASE
            WHEN TG_OP = 'DELETE' THEN OLD.turn_attempt_id
            ELSE NEW.turn_attempt_id
        END
    );
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER turn_attempt_requires_interrupt_proof
AFTER INSERT OR UPDATE OR DELETE ON turn_attempt
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_interrupt_attempt_proof();

DROP TRIGGER submit_input_command_requires_correlated_effect
    ON submit_input_command;

ALTER FUNCTION require_submit_input_effect_correlation()
    RENAME TO require_submit_input_legacy_effect_correlation;

CREATE FUNCTION require_interrupt_submit_input_effect_correlation()
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
                'interrupt_already_applied'
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
            'interrupt_already_applied'
        ),
        false
    )
)
EXECUTE FUNCTION require_interrupt_submit_input_effect_correlation();

CREATE OR REPLACE FUNCTION require_semantic_entry_turn_state()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    checked_payload_kind text;
    checked_source_session_id uuid;
    checked_origin_input_id uuid;
    checked_steering_source_turn_id uuid;
    checked_failed_turn_id uuid;
    checked_producing_call_id uuid;
    checked_completed_turn_id uuid;
    checked_cancelled_turn_id uuid;
    checked_turn_id uuid;
BEGIN
    IF TG_OP = 'DELETE' THEN
        checked_payload_kind := OLD.payload_kind;
        checked_source_session_id := OLD.source_session_id;
        checked_origin_input_id := OLD.origin_accepted_input_id;
        checked_steering_source_turn_id := OLD.steering_source_turn_id;
        checked_failed_turn_id := OLD.failed_turn_id;
        checked_producing_call_id := OLD.producing_model_call_id;
        checked_completed_turn_id := OLD.completed_turn_id;
        checked_cancelled_turn_id := OLD.cancelled_turn_id;
    ELSE
        checked_payload_kind := NEW.payload_kind;
        checked_source_session_id := NEW.source_session_id;
        checked_origin_input_id := NEW.origin_accepted_input_id;
        checked_steering_source_turn_id := NEW.steering_source_turn_id;
        checked_failed_turn_id := NEW.failed_turn_id;
        checked_producing_call_id := NEW.producing_model_call_id;
        checked_completed_turn_id := NEW.completed_turn_id;
        checked_cancelled_turn_id := NEW.cancelled_turn_id;
    END IF;

    CASE checked_payload_kind
        WHEN 'origin_accepted_input' THEN
            SELECT origin_turn_id
              INTO checked_turn_id
              FROM accepted_input
             WHERE accepted_input_id = checked_origin_input_id
               AND session_id = checked_source_session_id
               AND disposition_kind IN (
                    'origin_of',
                    'reclassified_as_turn_origin'
               )
               AND origin_turn_id IS NOT NULL;

            IF NOT FOUND THEN
                RAISE EXCEPTION 'semantic origin input % is not a turn origin',
                    checked_origin_input_id
                    USING
                        ERRCODE = '23514',
                        CONSTRAINT =
                            'semantic_transcript_entry_origin_disposition';
            END IF;
        WHEN 'steering_accepted_input' THEN
            SELECT expected_active_turn_id, consuming_model_call_id
              INTO checked_turn_id, checked_producing_call_id
              FROM accepted_input
             WHERE accepted_input_id = checked_origin_input_id
               AND session_id = checked_source_session_id
               AND disposition_kind = 'consumed_as_steering'
               AND expected_active_turn_id = checked_steering_source_turn_id
               AND consuming_model_call_id IS NOT NULL;

            IF NOT FOUND THEN
                RAISE EXCEPTION
                    'semantic steering input is not consumed by its source turn call'
                    USING ERRCODE = '23514';
            END IF;
        WHEN 'turn_failed' THEN
            checked_turn_id := checked_failed_turn_id;
        WHEN 'turn_completed' THEN
            checked_turn_id := checked_completed_turn_id;
        WHEN 'turn_cancelled' THEN
            checked_turn_id := checked_cancelled_turn_id;
        WHEN 'assistant_text' THEN
            SELECT turn_id
              INTO checked_turn_id
              FROM model_call
             WHERE model_call_id = checked_producing_call_id
               AND state_kind = 'terminal'
               AND terminal_disposition_kind = 'completed';

            IF NOT FOUND THEN
                RAISE EXCEPTION
                    'assistant text requires its outcome-authoritative completed call'
                    USING ERRCODE = '23514';
            END IF;
        ELSE
            RAISE EXCEPTION
                'semantic payload kind % lacks construction authority',
                checked_payload_kind
                USING ERRCODE = '23514';
    END CASE;

    PERFORM assert_turn_lifecycle_final_state(checked_turn_id);
    IF checked_producing_call_id IS NOT NULL THEN
        PERFORM assert_model_call_final_state(checked_producing_call_id);
    END IF;
    RETURN NULL;
END;
$$;

ALTER FUNCTION assert_model_call_final_state(uuid)
    RENAME TO assert_model_call_final_state_without_stop;

CREATE FUNCTION assert_stopped_model_call_final_state(
    checked_model_call_id uuid
)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    checked_turn uuid;
    checked_session uuid;
    checked_attempt uuid;
    checked_state text;
    checked_disposition text;
    attempt_state text;
    attempt_variant text;
    attempt_disposition text;
    attempt_interrupt_command uuid;
    turn_state text;
    active_phase text;
    current_attempt uuid;
    terminal_attempt uuid;
    terminal_call uuid;
    terminal_disposition text;
BEGIN
    SELECT
        call.turn_id,
        call.session_id,
        call.turn_attempt_id,
        call.state_kind,
        call.terminal_disposition_kind,
        attempt.state_kind,
        attempt.end_variant,
        attempt.end_disposition,
        attempt.interrupt_command_id,
        lifecycle.state_kind,
        lifecycle.active_phase_kind,
        lifecycle.current_attempt_id,
        lifecycle.terminal_attempt_id,
        lifecycle.terminal_model_call_id,
        lifecycle.terminal_disposition_kind
      INTO
        checked_turn,
        checked_session,
        checked_attempt,
        checked_state,
        checked_disposition,
        attempt_state,
        attempt_variant,
        attempt_disposition,
        attempt_interrupt_command,
        turn_state,
        active_phase,
        current_attempt,
        terminal_attempt,
        terminal_call,
        terminal_disposition
      FROM model_call AS call
      JOIN turn_attempt AS attempt
        ON attempt.turn_attempt_id = call.turn_attempt_id
       AND attempt.turn_id = call.turn_id
       AND attempt.session_id = call.session_id
      JOIN turn_lifecycle AS lifecycle
        ON lifecycle.turn_id = call.turn_id
       AND lifecycle.session_id = call.session_id
     WHERE call.model_call_id = checked_model_call_id;

    IF NOT FOUND THEN
        RAISE EXCEPTION 'stopped model call lacks its exact attempt and turn'
            USING ERRCODE = '23503';
    END IF;

    IF attempt_interrupt_command IS NOT NULL THEN
        PERFORM assert_interrupt_attempt_proof(checked_attempt);
    END IF;
    PERFORM assert_model_call_steering_final_state(checked_model_call_id);

    IF checked_state = 'cancellation_requested' THEN
        IF turn_state IS DISTINCT FROM 'active'
           OR active_phase IS DISTINCT FROM 'running'
           OR current_attempt IS DISTINCT FROM checked_attempt
           OR attempt_state IS DISTINCT FROM 'stop_requested'
           OR attempt_variant IS NOT NULL
           OR attempt_disposition IS NOT NULL
        THEN
            RAISE EXCEPTION
                'cancellation-requested call lacks its durable stop request'
                USING ERRCODE = '23514';
        END IF;
    ELSIF checked_state = 'terminal'
          AND checked_disposition = 'cancelled'
          AND attempt_disposition = 'cancelled'
    THEN
        IF turn_state IS DISTINCT FROM 'terminal'
           OR terminal_disposition IS DISTINCT FROM 'cancelled'
           OR terminal_attempt IS DISTINCT FROM checked_attempt
           OR terminal_call IS DISTINCT FROM checked_model_call_id
           OR attempt_state IS DISTINCT FROM 'ended'
           OR attempt_variant IS DISTINCT FROM 'after_cancellation'
        THEN
            RAISE EXCEPTION
                'cancelled call lacks its exact cancelled turn outcome'
                USING ERRCODE = '23514';
        END IF;
    ELSIF checked_state = 'terminal'
          AND checked_disposition = 'ambiguous'
          AND attempt_disposition IN ('ambiguous', 'lost')
    THEN
        IF turn_state IS DISTINCT FROM 'terminal'
           OR terminal_disposition IS DISTINCT FROM 'reconciliation_required'
           OR terminal_attempt IS DISTINCT FROM checked_attempt
           OR terminal_call IS DISTINCT FROM checked_model_call_id
           OR attempt_state IS DISTINCT FROM 'ended'
           OR attempt_variant NOT IN ('without_stop', 'after_cancellation')
           OR (
                attempt_variant = 'without_stop'
                AND attempt_interrupt_command IS NOT NULL
           )
           OR (
                attempt_variant = 'after_cancellation'
                AND attempt_interrupt_command IS NULL
           )
        THEN
            RAISE EXCEPTION
                'ambiguous stopped call lacks exact reconciliation outcome'
                USING ERRCODE = '23514';
        END IF;
    ELSE
        RAISE EXCEPTION 'unsupported stopped model-call state'
            USING ERRCODE = '23514';
    END IF;
END;
$$;

CREATE FUNCTION assert_model_call_final_state(
    checked_model_call_id uuid
)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    stopped_state boolean;
BEGIN
    SELECT EXISTS (
        SELECT 1
          FROM model_call AS call
          JOIN turn_attempt AS attempt
            ON attempt.turn_attempt_id = call.turn_attempt_id
           AND attempt.turn_id = call.turn_id
           AND attempt.session_id = call.session_id
          JOIN turn_lifecycle AS lifecycle
            ON lifecycle.turn_id = call.turn_id
           AND lifecycle.session_id = call.session_id
         WHERE call.model_call_id = checked_model_call_id
           AND (
                (
                    attempt.interrupt_command_id IS NOT NULL
                    AND call.state_kind = 'cancellation_requested'
                )
                OR (
                    call.state_kind = 'terminal'
                    AND (
                        (
                            call.terminal_disposition_kind = 'cancelled'
                            AND attempt.interrupt_command_id IS NOT NULL
                            AND attempt.end_disposition = 'cancelled'
                        )
                        OR (
                            call.terminal_disposition_kind = 'ambiguous'
                            AND attempt.end_disposition IN ('ambiguous', 'lost')
                            AND lifecycle.state_kind = 'terminal'
                            AND lifecycle.terminal_disposition_kind
                                = 'reconciliation_required'
                            AND lifecycle.terminal_model_call_id
                                = checked_model_call_id
                        )
                    )
                )
           )
    )
      INTO stopped_state;

    IF stopped_state THEN
        PERFORM assert_stopped_model_call_final_state(checked_model_call_id);
    ELSE
        PERFORM assert_model_call_final_state_without_stop(
            checked_model_call_id
        );
    END IF;
END;
$$;

-- Migration 202607220001 defined the complete lifecycle assertion before
-- interrupt priority existed, and 202607220004 retained it under this helper
-- name. Replace only its positional predecessor selection: ordinary work keeps
-- acceptance order, while an interrupt successor authenticates the exact
-- predecessor named by its queue-order proof. The checked source function is
-- fixed by the preceding migrations; fail migration rather than silently
-- accepting an unexpected definition.
DO $migration$
DECLARE
    lifecycle_definition text;
    updated_definition text;
    positional_selection CONSTANT text := $old$
        SELECT max(acceptance_position)
          INTO expected_predecessor_position
          FROM turn_lifecycle
         WHERE session_id = checked_session_id
           AND acceptance_position < checked_position;
$old$;
    priority_selection CONSTANT text := $new$
        SELECT acceptance_position
          INTO expected_predecessor_position
          FROM turn_lifecycle
         WHERE session_id = checked_session_id
           AND turn_id = accepted_input_turn_queue_predecessor(
                checked_session_id,
                checked_turn_id
           );
$new$;
BEGIN
    SELECT pg_get_functiondef(
        'assert_turn_lifecycle_final_state_without_steering(uuid)'::regprocedure
    )
      INTO lifecycle_definition;
    updated_definition := replace(
        lifecycle_definition,
        positional_selection,
        priority_selection
    );
    IF updated_definition = lifecycle_definition THEN
        RAISE EXCEPTION
            'interrupt priority could not update lifecycle predecessor assertion';
    END IF;
    EXECUTE updated_definition;
END;
$migration$;

ALTER FUNCTION assert_turn_lifecycle_final_state(uuid)
    RENAME TO assert_turn_lifecycle_final_state_without_cancellation;

CREATE FUNCTION assert_cancelled_turn_final_state(
    checked_turn_id uuid
)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    checked_session uuid;
    checked_starting_frontier uuid;
    checked_terminal_frontier uuid;
    checked_terminal_attempt uuid;
    checked_terminal_call uuid;
    base_frontier uuid;
    base_member_count numeric(20, 0);
    terminal_member_count numeric(20, 0);
    prefix_mismatch_count bigint;
    checked_cancellation_entry uuid;
    cancellation_entry_count bigint;
    contradictory_entry_count bigint;
    call_count bigint;
    outbox_count bigint;
BEGIN
    SELECT
        session_id,
        starting_frontier_id,
        terminal_frontier_id,
        terminal_attempt_id,
        terminal_model_call_id
      INTO
        checked_session,
        checked_starting_frontier,
        checked_terminal_frontier,
        checked_terminal_attempt,
        checked_terminal_call
      FROM turn_lifecycle
     WHERE turn_id = checked_turn_id
       AND state_kind = 'terminal'
       AND terminal_disposition_kind = 'cancelled';

    IF NOT FOUND THEN
        RETURN;
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM turn_attempt
         WHERE turn_attempt_id = checked_terminal_attempt
           AND turn_id = checked_turn_id
           AND session_id = checked_session
           AND state_kind = 'ended'
           AND end_variant = 'after_cancellation'
           AND end_disposition = 'cancelled'
    ) THEN
        RAISE EXCEPTION 'cancelled turn lacks its exact ended attempt'
            USING ERRCODE = '23514';
    END IF;
    PERFORM assert_interrupt_attempt_proof(checked_terminal_attempt);

    SELECT count(*)
      INTO call_count
      FROM model_call
     WHERE turn_id = checked_turn_id
       AND session_id = checked_session;

    IF checked_terminal_call IS NULL THEN
        IF call_count <> 0 THEN
            RAISE EXCEPTION 'directly cancelled turn names no call but stores one'
                USING ERRCODE = '23514';
        END IF;
        base_frontier := checked_starting_frontier;
    ELSE
        IF call_count <> 1
           OR NOT EXISTS (
                SELECT 1
                  FROM model_call
                 WHERE model_call_id = checked_terminal_call
                   AND turn_attempt_id = checked_terminal_attempt
                   AND turn_id = checked_turn_id
                   AND session_id = checked_session
                   AND state_kind = 'terminal'
                   AND terminal_disposition_kind = 'cancelled'
           )
        THEN
            RAISE EXCEPTION 'cancelled turn lacks its exact cancelled call'
                USING ERRCODE = '23514';
        END IF;
        SELECT context_frontier_id
          INTO base_frontier
          FROM model_call
         WHERE model_call_id = checked_terminal_call;
        PERFORM assert_model_call_final_state(checked_terminal_call);
    END IF;

    SELECT count(*)
      INTO cancellation_entry_count
      FROM semantic_transcript_entry
     WHERE source_session_id = checked_session
       AND payload_kind = 'turn_cancelled'
       AND cancelled_turn_id = checked_turn_id;
    SELECT semantic_entry_id
      INTO checked_cancellation_entry
      FROM semantic_transcript_entry
     WHERE source_session_id = checked_session
       AND payload_kind = 'turn_cancelled'
       AND cancelled_turn_id = checked_turn_id
     ORDER BY semantic_entry_id
     LIMIT 1;

    SELECT count(*)
      INTO contradictory_entry_count
      FROM semantic_transcript_entry
     WHERE source_session_id = checked_session
       AND (
            failed_turn_id = checked_turn_id
            OR completed_turn_id = checked_turn_id
            OR producing_model_call_id = checked_terminal_call
       )
       AND payload_kind IN (
            'turn_failed',
            'turn_completed',
            'assistant_text'
       );

    SELECT member_count
      INTO base_member_count
      FROM context_frontier
     WHERE owning_session_id = checked_session
       AND context_frontier_id = base_frontier;
    SELECT member_count
      INTO terminal_member_count
      FROM context_frontier
     WHERE owning_session_id = checked_session
       AND context_frontier_id = checked_terminal_frontier;

    SELECT count(*)
      INTO prefix_mismatch_count
      FROM context_frontier_member AS base_member
      LEFT JOIN context_frontier_member AS terminal_member
        ON terminal_member.owning_session_id = base_member.owning_session_id
       AND terminal_member.context_frontier_id = checked_terminal_frontier
       AND terminal_member.member_position = base_member.member_position
       AND terminal_member.source_session_id = base_member.source_session_id
       AND terminal_member.semantic_entry_id = base_member.semantic_entry_id
     WHERE base_member.owning_session_id = checked_session
       AND base_member.context_frontier_id = base_frontier
       AND terminal_member.member_position IS NULL;

    SELECT count(*)
      INTO outbox_count
      FROM turn_cancelled_outbox_event
     WHERE session_id = checked_session
       AND turn_id = checked_turn_id
       AND cancellation_entry_id = checked_cancellation_entry
       AND terminal_frontier_id = checked_terminal_frontier;

    IF cancellation_entry_count <> 1
       OR contradictory_entry_count <> 0
       OR base_member_count IS NULL
       OR terminal_member_count IS DISTINCT FROM base_member_count + 1
       OR prefix_mismatch_count <> 0
       OR NOT EXISTS (
            SELECT 1
              FROM context_frontier_member
             WHERE owning_session_id = checked_session
               AND context_frontier_id = checked_terminal_frontier
               AND member_position = terminal_member_count
               AND source_session_id = checked_session
               AND semantic_entry_id = checked_cancellation_entry
       )
       OR outbox_count <> 1
    THEN
        RAISE EXCEPTION
            'cancelled turn lacks its exact semantic, frontier, or outbox boundary'
            USING ERRCODE = '23514';
    END IF;
END;
$$;

CREATE FUNCTION assert_turn_lifecycle_final_state(
    checked_turn_id uuid
)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    cancelled_terminal boolean;
BEGIN
    SELECT EXISTS (
        SELECT 1
          FROM turn_lifecycle
         WHERE turn_id = checked_turn_id
           AND state_kind = 'terminal'
           AND terminal_disposition_kind = 'cancelled'
    )
      INTO cancelled_terminal;

    IF cancelled_terminal THEN
        PERFORM assert_cancelled_turn_final_state(checked_turn_id);
    ELSE
        PERFORM assert_turn_lifecycle_final_state_without_cancellation(
            checked_turn_id
        );
    END IF;
END;
$$;

ALTER FUNCTION assert_failed_terminal_execution_final_state(uuid)
    RENAME TO assert_failed_terminal_execution_without_cancellation;

CREATE FUNCTION assert_failed_terminal_execution_final_state(
    checked_turn_id uuid
)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    checked_session uuid;
    checked_attempt uuid;
    checked_call uuid;
    cancellation_failure boolean;
BEGIN
    SELECT
        lifecycle.session_id,
        lifecycle.terminal_attempt_id,
        lifecycle.terminal_model_call_id,
        EXISTS (
            SELECT 1
              FROM turn_attempt AS attempt
             WHERE attempt.turn_attempt_id = lifecycle.terminal_attempt_id
               AND attempt.turn_id = lifecycle.turn_id
               AND attempt.session_id = lifecycle.session_id
               AND attempt.end_variant = 'after_cancellation'
               AND attempt.end_disposition IN ('known_failure', 'lost')
        )
      INTO
        checked_session,
        checked_attempt,
        checked_call,
        cancellation_failure
      FROM turn_lifecycle AS lifecycle
     WHERE lifecycle.turn_id = checked_turn_id
       AND lifecycle.state_kind = 'terminal'
       AND lifecycle.terminal_disposition_kind = 'failed';

    IF NOT FOUND OR NOT cancellation_failure THEN
        PERFORM assert_failed_terminal_execution_without_cancellation(
            checked_turn_id
        );
        RETURN;
    END IF;

    PERFORM assert_interrupt_attempt_proof(checked_attempt);
    IF checked_call IS NOT NULL THEN
        IF NOT EXISTS (
            SELECT 1
              FROM model_call
             WHERE model_call_id = checked_call
               AND turn_attempt_id = checked_attempt
               AND turn_id = checked_turn_id
               AND session_id = checked_session
               AND state_kind = 'terminal'
               AND terminal_disposition_kind = 'known_failed'
        ) THEN
            RAISE EXCEPTION
                'post-cancellation failure lacks its exact terminal call'
                USING ERRCODE = '23514';
        END IF;
        PERFORM assert_model_call_final_state(checked_call);
    END IF;
END;
$$;

ALTER FUNCTION assert_turn_lifecycle_final_state(uuid)
    RENAME TO assert_turn_lifecycle_final_state_without_reconciliation;

CREATE FUNCTION assert_terminal_started_turn_common_final_state(
    checked_turn_id uuid
)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    checked_session uuid;
    checked_origin_input uuid;
    checked_position numeric(20, 0);
    checked_attempt_history boolean;
    checked_lineage text;
    checked_predecessor uuid;
    checked_starting_frontier uuid;
    checked_terminal_attempt uuid;
    attempt_count bigint;
    ended_attempt_count bigint;
    origin_entry_count bigint;
    origin_entry uuid;
    starting_member_count numeric(20, 0);
    origin_member_count bigint;
    origin_member_position numeric(20, 0);
    predecessor_turn uuid;
    predecessor_frontier uuid;
    predecessor_member_count numeric(20, 0);
    prefix_mismatch_count bigint;
BEGIN
    SELECT
        session_id,
        origin_accepted_input_id,
        acceptance_position,
        attempt_history_present,
        start_lineage_kind,
        immediate_predecessor_turn_id,
        starting_frontier_id,
        terminal_attempt_id
      INTO
        checked_session,
        checked_origin_input,
        checked_position,
        checked_attempt_history,
        checked_lineage,
        checked_predecessor,
        checked_starting_frontier,
        checked_terminal_attempt
      FROM turn_lifecycle
     WHERE turn_id = checked_turn_id
       AND state_kind = 'terminal';

    IF NOT FOUND THEN
        RETURN;
    END IF;

    SELECT
        count(*),
        count(*) FILTER (
            WHERE state_kind = 'ended'
              AND turn_attempt_id = checked_terminal_attempt
        )
      INTO attempt_count, ended_attempt_count
      FROM turn_attempt
     WHERE turn_id = checked_turn_id
       AND session_id = checked_session;

    IF checked_attempt_history IS DISTINCT FROM (attempt_count > 0)
       OR attempt_count <> 1
       OR ended_attempt_count <> 1
    THEN
        RAISE EXCEPTION
            'terminal turn % lacks its exact single ended attempt history',
            checked_turn_id
            USING ERRCODE = '23514';
    END IF;

    SELECT count(*)
      INTO origin_entry_count
      FROM semantic_transcript_entry
     WHERE source_session_id = checked_session
       AND payload_kind = 'origin_accepted_input'
       AND origin_accepted_input_id = checked_origin_input;
    IF origin_entry_count <> 1 THEN
        RAISE EXCEPTION
            'terminal turn % lacks its exact origin entry',
            checked_turn_id
            USING ERRCODE = '23514';
    END IF;
    SELECT semantic_entry_id
      INTO origin_entry
      FROM semantic_transcript_entry
     WHERE source_session_id = checked_session
       AND payload_kind = 'origin_accepted_input'
       AND origin_accepted_input_id = checked_origin_input;

    SELECT member_count
      INTO starting_member_count
      FROM context_frontier
     WHERE owning_session_id = checked_session
       AND context_frontier_id = checked_starting_frontier;
    SELECT count(*), max(member_position)
      INTO origin_member_count, origin_member_position
      FROM context_frontier_member
     WHERE owning_session_id = checked_session
       AND context_frontier_id = checked_starting_frontier
       AND source_session_id = checked_session
       AND semantic_entry_id = origin_entry;
    IF starting_member_count IS NULL
       OR origin_member_count <> 1
       OR origin_member_position IS DISTINCT FROM starting_member_count
    THEN
        RAISE EXCEPTION
            'terminal turn % starting frontier lacks its final origin',
            checked_turn_id
            USING ERRCODE = '23514';
    END IF;

    predecessor_turn := accepted_input_turn_queue_predecessor(
        checked_session,
        checked_turn_id
    );
    IF checked_lineage = 'first_in_session' THEN
        IF checked_predecessor IS NOT NULL
           OR predecessor_turn IS NOT NULL
           OR starting_member_count <> 1
        THEN
            RAISE EXCEPTION
                'terminal turn % has inconsistent first lineage',
                checked_turn_id
                USING ERRCODE = '23514';
        END IF;
    ELSIF checked_lineage = 'after' THEN
        IF checked_predecessor IS DISTINCT FROM predecessor_turn THEN
            RAISE EXCEPTION
                'terminal turn % does not name its queue predecessor',
                checked_turn_id
                USING ERRCODE = '23514';
        END IF;
        SELECT terminal_frontier_id
          INTO predecessor_frontier
          FROM turn_lifecycle
         WHERE turn_id = checked_predecessor
           AND session_id = checked_session
           AND state_kind = 'terminal';
        IF NOT FOUND THEN
            RAISE EXCEPTION
                'terminal turn % predecessor is not terminal',
                checked_turn_id
                USING ERRCODE = '23514';
        END IF;
        SELECT member_count
          INTO predecessor_member_count
          FROM context_frontier
         WHERE owning_session_id = checked_session
           AND context_frontier_id = predecessor_frontier;
        SELECT count(*)
          INTO prefix_mismatch_count
          FROM context_frontier_member AS predecessor_member
          LEFT JOIN context_frontier_member AS starting_member
            ON starting_member.owning_session_id = checked_session
           AND starting_member.context_frontier_id = checked_starting_frontier
           AND starting_member.member_position = predecessor_member.member_position
           AND starting_member.source_session_id = predecessor_member.source_session_id
           AND starting_member.semantic_entry_id = predecessor_member.semantic_entry_id
         WHERE predecessor_member.owning_session_id = checked_session
           AND predecessor_member.context_frontier_id = predecessor_frontier
           AND starting_member.member_position IS NULL;
        IF starting_member_count
               IS DISTINCT FROM predecessor_member_count + 1
           OR prefix_mismatch_count <> 0
        THEN
            RAISE EXCEPTION
                'terminal turn % starting frontier does not extend its predecessor',
                checked_turn_id
                USING ERRCODE = '23514';
        END IF;
    ELSE
        RAISE EXCEPTION
            'terminal turn % has unsupported lineage',
            checked_turn_id
            USING ERRCODE = '23514';
    END IF;
END;
$$;

CREATE FUNCTION assert_reconciliation_required_turn_final_state(
    checked_turn_id uuid
)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    checked_session uuid;
    checked_attempt uuid;
    checked_call uuid;
    checked_terminal_frontier uuid;
    source_frontier uuid;
    member_mismatch_count bigint;
    contradictory_entry_count bigint;
    outbox_count bigint;
    interrupt_command uuid;
    interrupt_record_count bigint;
BEGIN
    SELECT
        session_id,
        terminal_attempt_id,
        terminal_model_call_id,
        terminal_frontier_id
      INTO
        checked_session,
        checked_attempt,
        checked_call,
        checked_terminal_frontier
      FROM turn_lifecycle
     WHERE turn_id = checked_turn_id
       AND state_kind = 'terminal'
       AND terminal_disposition_kind = 'reconciliation_required';

    IF NOT FOUND THEN
        RETURN;
    END IF;

    PERFORM assert_terminal_started_turn_common_final_state(checked_turn_id);

    IF NOT EXISTS (
        SELECT 1
          FROM turn_attempt
         WHERE turn_attempt_id = checked_attempt
           AND turn_id = checked_turn_id
           AND session_id = checked_session
           AND state_kind = 'ended'
           AND end_disposition IN ('ambiguous', 'lost')
           AND (
                (
                    end_variant = 'after_cancellation'
                    AND interrupt_command_id IS NOT NULL
                    AND interrupt_predecessor_turn_id = checked_turn_id
                )
                OR (
                    end_variant = 'without_stop'
                    AND interrupt_command_id IS NULL
                    AND interrupt_predecessor_turn_id IS NULL
                )
           )
    ) THEN
        RAISE EXCEPTION
            'reconciliation-required turn lacks exact ambiguous attempt'
            USING ERRCODE = '23514';
    END IF;

    SELECT interrupt_command_id
      INTO interrupt_command
      FROM turn_attempt
     WHERE turn_attempt_id = checked_attempt;
    IF interrupt_command IS NOT NULL THEN
        PERFORM assert_interrupt_attempt_proof(checked_attempt);
    END IF;

    SELECT count(*)
      INTO interrupt_record_count
      FROM submit_input_command AS command
      JOIN accepted_input AS accepted
        ON accepted.accepting_command_id = command.command_id
       AND accepted.accepted_input_id = command.result_accepted_input_id
       AND accepted.session_id = command.result_session_id
       AND accepted.origin_turn_id = command.result_turn_id
      JOIN queued_input_origin AS successor
        ON successor.accepted_input_id = accepted.accepted_input_id
       AND successor.turn_id = accepted.origin_turn_id
       AND successor.session_id = accepted.session_id
       AND successor.priority_kind = 'interrupt_immediately_after'
       AND successor.interrupt_predecessor_turn_id = checked_turn_id
     WHERE command.session_id = checked_session
       AND command.delivery_kind = 'interrupt'
       AND command.expected_active_turn_id = checked_turn_id
       AND command.result_kind = 'applied'
       AND command.rejection_kind IS NULL
       AND accepted.disposition_kind = 'origin_of'
       AND (
            interrupt_command IS NULL
            OR command.command_id = interrupt_command
       );
    IF interrupt_record_count <> 1 THEN
        RAISE EXCEPTION
            'reconciliation-required turn lacks its exact applied interrupt'
            USING ERRCODE = '23514';
    END IF;

    SELECT context_frontier_id
      INTO source_frontier
      FROM model_call
     WHERE model_call_id = checked_call
       AND turn_attempt_id = checked_attempt
       AND turn_id = checked_turn_id
       AND session_id = checked_session
       AND state_kind = 'terminal'
       AND terminal_disposition_kind = 'ambiguous';
    IF NOT FOUND THEN
        RAISE EXCEPTION
            'reconciliation-required turn lacks exact ambiguous call'
            USING ERRCODE = '23514';
    END IF;
    PERFORM assert_model_call_final_state(checked_call);

    SELECT count(*)
      INTO member_mismatch_count
      FROM (
            (
                SELECT member_position, source_session_id, semantic_entry_id
                  FROM context_frontier_member
                 WHERE owning_session_id = checked_session
                   AND context_frontier_id = source_frontier
                EXCEPT
                SELECT member_position, source_session_id, semantic_entry_id
                  FROM context_frontier_member
                 WHERE owning_session_id = checked_session
                   AND context_frontier_id = checked_terminal_frontier
            )
            UNION ALL
            (
                SELECT member_position, source_session_id, semantic_entry_id
                  FROM context_frontier_member
                 WHERE owning_session_id = checked_session
                   AND context_frontier_id = checked_terminal_frontier
                EXCEPT
                SELECT member_position, source_session_id, semantic_entry_id
                  FROM context_frontier_member
                 WHERE owning_session_id = checked_session
                   AND context_frontier_id = source_frontier
            )
      ) AS mismatch;

    SELECT count(*)
      INTO contradictory_entry_count
      FROM semantic_transcript_entry
     WHERE source_session_id = checked_session
       AND (
            failed_turn_id = checked_turn_id
            OR completed_turn_id = checked_turn_id
            OR cancelled_turn_id = checked_turn_id
            OR producing_model_call_id = checked_call
       )
       AND payload_kind IN (
            'turn_failed',
            'turn_completed',
            'turn_cancelled',
            'assistant_text'
       );

    SELECT count(*)
      INTO outbox_count
      FROM turn_reconciliation_required_outbox_event
     WHERE session_id = checked_session
       AND turn_id = checked_turn_id
       AND model_call_id = checked_call
       AND terminal_frontier_id = checked_terminal_frontier;

    IF member_mismatch_count <> 0
       OR contradictory_entry_count <> 0
       OR outbox_count <> 1
    THEN
        RAISE EXCEPTION
            'reconciliation-required turn lacks exact frontier or outbox boundary'
            USING ERRCODE = '23514';
    END IF;
END;
$$;

CREATE FUNCTION assert_turn_lifecycle_final_state(
    checked_turn_id uuid
)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    reconciliation_terminal boolean;
BEGIN
    SELECT EXISTS (
        SELECT 1
          FROM turn_lifecycle
         WHERE turn_id = checked_turn_id
           AND state_kind = 'terminal'
           AND terminal_disposition_kind = 'reconciliation_required'
    )
      INTO reconciliation_terminal;

    IF reconciliation_terminal THEN
        PERFORM assert_reconciliation_required_turn_final_state(
            checked_turn_id
        );
    ELSE
        PERFORM assert_turn_lifecycle_final_state_without_reconciliation(
            checked_turn_id
        );
    END IF;
END;
$$;
