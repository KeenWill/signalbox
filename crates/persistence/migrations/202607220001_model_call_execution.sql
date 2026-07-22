-- ADR-0042/ADR-0043/ADR-0045 storage for the first text-only model call.
--
-- The migration opens the existing guarded turn storage only for the decided
-- initial execution states. Provider work remains outside transactions; these
-- rows record the Prepared checkpoint, send authorization, terminal evidence,
-- and atomic conversational outcome that surround that work.

-- A pending steering receipt keeps its immutable NextSafePoint delivery and
-- command result when terminalization makes it ordinary successor work. Only
-- its current disposition and fresh origin turn change; the source binding
-- remains in expected_active_turn_id.
ALTER TABLE accepted_input
    DROP CONSTRAINT accepted_input_delivery_shape,
    DROP CONSTRAINT accepted_input_disposition_closed,
    DROP CONSTRAINT accepted_input_command_result_fk;

-- The immutable command receipt continues to own accepted-input identity and
-- session. Its original result turn remains authoritative for direct origins,
-- while a pending-steering receipt deliberately has no result turn to copy
-- when later terminalization gives the accepted input a successor turn.
ALTER TABLE submit_input_command
    ADD CONSTRAINT submit_input_command_accepted_result_key
        UNIQUE (command_id, result_accepted_input_id, result_session_id);

ALTER TABLE accepted_input
    ADD CONSTRAINT accepted_input_command_result_fk
        FOREIGN KEY (accepting_command_id, accepted_input_id, session_id)
        REFERENCES submit_input_command (
            command_id,
            result_accepted_input_id,
            result_session_id
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED;

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
        )
    ),
    ADD CONSTRAINT accepted_input_disposition_closed
    CHECK (
        disposition_kind IN (
            'origin_of',
            'pending_steering',
            'reclassified_as_turn_origin'
        )
    );

DROP TRIGGER accepted_input_is_append_only ON accepted_input;

CREATE FUNCTION reject_invalid_accepted_input_change()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'accepted_input is not deletable'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.disposition_kind = 'pending_steering'
       AND NEW.disposition_kind = 'reclassified_as_turn_origin'
       AND OLD.origin_turn_id IS NULL
       AND NEW.origin_turn_id IS NOT NULL
       AND ROW(
            OLD.accepted_input_id,
            OLD.accepting_command_id,
            OLD.session_id,
            OLD.content_kind,
            OLD.content_text,
            OLD.delivery_kind,
            OLD.expected_active_turn_id,
            OLD.expected_defaults_version,
            OLD.model_override_kind,
            OLD.replacement_model_kind,
            OLD.replacement_direct_model_selection_id,
            OLD.replacement_model_alias_id,
            OLD.acceptance_position
       ) IS NOT DISTINCT FROM ROW(
            NEW.accepted_input_id,
            NEW.accepting_command_id,
            NEW.session_id,
            NEW.content_kind,
            NEW.content_text,
            NEW.delivery_kind,
            NEW.expected_active_turn_id,
            NEW.expected_defaults_version,
            NEW.model_override_kind,
            NEW.replacement_model_kind,
            NEW.replacement_direct_model_selection_id,
            NEW.replacement_model_alias_id,
            NEW.acceptance_position
       )
    THEN
        RETURN NEW;
    END IF;

    RAISE EXCEPTION 'accepted_input is immutable outside pending-steering reclassification'
        USING ERRCODE = '23514';
END;
$$;

CREATE TRIGGER accepted_input_is_append_only
BEFORE UPDATE OR DELETE ON accepted_input
FOR EACH ROW
EXECUTE FUNCTION reject_invalid_accepted_input_change();

-- Reclassified steering inherits the effective configuration of its canonical
-- source turn. Its queue row stores only that source reference; it never
-- duplicates configuration values that could compete with the source fact.
ALTER TABLE queued_input_origin
    ADD COLUMN source_configuration_turn_id uuid,
    ALTER COLUMN defaults_version DROP NOT NULL,
    ALTER COLUMN requested_model_kind DROP NOT NULL,
    ALTER COLUMN frozen_model_kind DROP NOT NULL,
    ALTER COLUMN model_parameters DROP NOT NULL,
    ALTER COLUMN known_provider_failure_retry DROP NOT NULL,
    ALTER COLUMN model_fallback DROP NOT NULL,
    DROP CONSTRAINT queued_input_origin_defaults_version_positive_u64,
    DROP CONSTRAINT queued_input_origin_requested_model_shape,
    DROP CONSTRAINT queued_input_origin_frozen_model_shape,
    DROP CONSTRAINT queued_input_origin_model_parameters_closed,
    DROP CONSTRAINT queued_input_origin_known_failure_retry_closed,
    DROP CONSTRAINT queued_input_origin_model_fallback_closed;

ALTER TABLE queued_input_origin
    ADD CONSTRAINT queued_input_origin_configuration_provenance_shape
        CHECK (
            (
                source_configuration_turn_id IS NULL
                AND defaults_version IS NOT NULL
                AND requested_model_kind IS NOT NULL
                AND frozen_model_kind IS NOT NULL
                AND model_parameters IS NOT NULL
                AND known_provider_failure_retry IS NOT NULL
                AND model_fallback IS NOT NULL
            )
            OR
            (
                source_configuration_turn_id IS NOT NULL
                AND defaults_version IS NULL
                AND requested_model_kind IS NULL
                AND requested_direct_model_selection_id IS NULL
                AND requested_model_alias_id IS NULL
                AND frozen_model_kind IS NULL
                AND frozen_direct_model_selection_id IS NULL
                AND frozen_model_alias_id IS NULL
                AND frozen_alias_selected_direct_id IS NULL
                AND model_parameters IS NULL
                AND known_provider_failure_retry IS NULL
                AND model_fallback IS NULL
            )
        ),
    ADD CONSTRAINT queued_input_origin_defaults_version_positive_u64
        CHECK (
            defaults_version IS NULL
            OR (
                defaults_version >= 1
                AND defaults_version <= 18446744073709551615
            )
        ),
    ADD CONSTRAINT queued_input_origin_requested_model_shape
        CHECK (
            source_configuration_turn_id IS NOT NULL
            OR (
                requested_model_kind = 'direct'
                AND requested_direct_model_selection_id IS NOT NULL
                AND requested_model_alias_id IS NULL
            )
            OR (
                requested_model_kind = 'alias'
                AND requested_direct_model_selection_id IS NULL
                AND requested_model_alias_id IS NOT NULL
            )
        ),
    ADD CONSTRAINT queued_input_origin_frozen_model_shape
        CHECK (
            source_configuration_turn_id IS NOT NULL
            OR (
                frozen_model_kind = 'direct'
                AND frozen_direct_model_selection_id IS NOT NULL
                AND frozen_model_alias_id IS NULL
                AND frozen_alias_selected_direct_id IS NULL
            )
            OR (
                frozen_model_kind = 'frozen_alias'
                AND frozen_direct_model_selection_id IS NULL
                AND frozen_model_alias_id IS NOT NULL
                AND frozen_alias_selected_direct_id IS NOT NULL
            )
        ),
    ADD CONSTRAINT queued_input_origin_model_parameters_closed
        CHECK (
            source_configuration_turn_id IS NOT NULL
            OR model_parameters = 'provider_defaults'
        ),
    ADD CONSTRAINT queued_input_origin_known_failure_retry_closed
        CHECK (
            source_configuration_turn_id IS NOT NULL
            OR known_provider_failure_retry = 'disabled'
        ),
    ADD CONSTRAINT queued_input_origin_model_fallback_closed
        CHECK (
            source_configuration_turn_id IS NOT NULL
            OR model_fallback = 'disabled'
        ),
    ADD CONSTRAINT queued_input_origin_source_not_self
        CHECK (source_configuration_turn_id IS DISTINCT FROM turn_id),
    ADD CONSTRAINT queued_input_origin_turn_session_key
        UNIQUE (turn_id, session_id),
    ADD CONSTRAINT queued_input_origin_configuration_source_fk
        FOREIGN KEY (source_configuration_turn_id, session_id)
        REFERENCES queued_input_origin (turn_id, session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED;

-- ADR-0042 makes the provider target a turn-level pin established before any
-- physical call. Calls reference this fact rather than serving as its storage.
ALTER TABLE turn_lifecycle
    ADD COLUMN pinned_provider_model_identity_id uuid,
    ADD CONSTRAINT turn_lifecycle_pinned_target_key
        UNIQUE (turn_id, session_id, pinned_provider_model_identity_id);

CREATE TABLE model_call (
    model_call_id uuid PRIMARY KEY,
    turn_id uuid NOT NULL,
    session_id uuid NOT NULL,
    turn_attempt_id uuid NOT NULL,
    selection_kind text NOT NULL,
    direct_model_selection_id uuid,
    frozen_model_alias_id uuid,
    frozen_alias_selected_direct_id uuid,
    resolved_provider_model_identity_id uuid NOT NULL,
    context_frontier_id uuid NOT NULL,
    state_kind text NOT NULL,
    terminal_disposition_kind text,

    CONSTRAINT model_call_selection_kind_closed
        CHECK (selection_kind IN ('direct', 'frozen_alias')),
    CONSTRAINT model_call_selection_shape
        CHECK (
            (
                selection_kind = 'direct'
                AND direct_model_selection_id IS NOT NULL
                AND frozen_model_alias_id IS NULL
                AND frozen_alias_selected_direct_id IS NULL
            )
            OR
            (
                selection_kind = 'frozen_alias'
                AND direct_model_selection_id IS NULL
                AND frozen_model_alias_id IS NOT NULL
                AND frozen_alias_selected_direct_id IS NOT NULL
            )
        ),
    CONSTRAINT model_call_state_kind_closed
        CHECK (
            state_kind IN (
                'prepared',
                'in_flight',
                'cancellation_requested',
                'terminal'
            )
        ),
    CONSTRAINT model_call_terminal_disposition_closed
        CHECK (
            terminal_disposition_kind IS NULL
            OR terminal_disposition_kind IN (
                'completed',
                'known_failed',
                'refused',
                'cancelled',
                'ambiguous'
            )
        ),
    CONSTRAINT model_call_state_payload_shape
        CHECK (
            (
                state_kind <> 'terminal'
                AND terminal_disposition_kind IS NULL
            )
            OR
            (
                state_kind = 'terminal'
                AND terminal_disposition_kind IS NOT NULL
            )
        ),
    CONSTRAINT model_call_turn_correlation_key
        UNIQUE (model_call_id, turn_id, session_id),
    CONSTRAINT model_call_session_correlation_key
        UNIQUE (model_call_id, session_id),
    CONSTRAINT model_call_attempt_once
        UNIQUE (turn_attempt_id),
    CONSTRAINT model_call_attempt_fk
        FOREIGN KEY (turn_attempt_id, turn_id, session_id)
        REFERENCES turn_attempt (turn_attempt_id, turn_id, session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT model_call_pinned_target_fk
        FOREIGN KEY (
            turn_id,
            session_id,
            resolved_provider_model_identity_id
        )
        REFERENCES turn_lifecycle (
            turn_id,
            session_id,
            pinned_provider_model_identity_id
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT model_call_frontier_fk
        FOREIGN KEY (session_id, context_frontier_id)
        REFERENCES context_frontier (owning_session_id, context_frontier_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE INDEX model_call_by_turn_attempt
    ON model_call (turn_id, turn_attempt_id);

CREATE FUNCTION reject_model_call_invalid_change()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        IF NEW.state_kind <> 'prepared' THEN
            RAISE EXCEPTION 'model call must be inserted as Prepared'
                USING
                    ERRCODE = '23514',
                    CONSTRAINT = 'model_call_inserted_prepared';
        END IF;
        RETURN NEW;
    END IF;

    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'model_call is not deletable'
            USING ERRCODE = '23514';
    END IF;

    IF ROW(
        OLD.model_call_id,
        OLD.turn_id,
        OLD.session_id,
        OLD.turn_attempt_id,
        OLD.selection_kind,
        OLD.direct_model_selection_id,
        OLD.frozen_model_alias_id,
        OLD.frozen_alias_selected_direct_id,
        OLD.resolved_provider_model_identity_id,
        OLD.context_frontier_id
    ) IS DISTINCT FROM ROW(
        NEW.model_call_id,
        NEW.turn_id,
        NEW.session_id,
        NEW.turn_attempt_id,
        NEW.selection_kind,
        NEW.direct_model_selection_id,
        NEW.frozen_model_alias_id,
        NEW.frozen_alias_selected_direct_id,
        NEW.resolved_provider_model_identity_id,
        NEW.context_frontier_id
    ) THEN
        RAISE EXCEPTION 'model call authorization facts are immutable'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.state_kind = 'terminal' THEN
        RAISE EXCEPTION 'terminal model call is immutable'
            USING ERRCODE = '23514';
    END IF;

    IF NOT (
        OLD.state_kind = NEW.state_kind
        OR (
            OLD.state_kind = 'prepared'
            AND NEW.state_kind IN ('in_flight', 'terminal')
        )
        OR (
            OLD.state_kind = 'in_flight'
            AND NEW.state_kind IN ('cancellation_requested', 'terminal')
        )
        OR (
            OLD.state_kind = 'cancellation_requested'
            AND NEW.state_kind = 'terminal'
        )
    ) THEN
        RAISE EXCEPTION 'model call transition is not monotonic'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.state_kind = 'prepared'
       AND NEW.state_kind = 'terminal'
       AND NEW.terminal_disposition_kind NOT IN ('known_failed', 'cancelled')
    THEN
        RAISE EXCEPTION 'an unsent call has an impossible terminal disposition'
            USING ERRCODE = '23514';
    END IF;

    RETURN NEW;
END;
$$;

CREATE TRIGGER model_call_changes_are_guarded
BEFORE INSERT OR UPDATE OR DELETE ON model_call
FOR EACH ROW
EXECUTE FUNCTION reject_model_call_invalid_change();

ALTER TABLE semantic_transcript_entry
    ADD COLUMN assistant_text_value text,
    ADD COLUMN producing_model_call_id uuid,
    ADD COLUMN assistant_tool_request_id uuid,
    ADD COLUMN completed_turn_id uuid;

ALTER TABLE semantic_transcript_entry
    DROP CONSTRAINT semantic_transcript_entry_payload_kind_closed,
    DROP CONSTRAINT semantic_transcript_entry_payload_shape;

ALTER TABLE semantic_transcript_entry
    ADD CONSTRAINT semantic_transcript_entry_payload_kind_closed
        CHECK (
            payload_kind IN (
                'origin_accepted_input',
                'turn_failed',
                'assistant_text',
                'assistant_tool_use',
                'turn_completed'
            )
        ),
    ADD CONSTRAINT semantic_transcript_entry_payload_shape
        CHECK (
            (
                payload_kind = 'origin_accepted_input'
                AND origin_accepted_input_id IS NOT NULL
                AND failed_turn_id IS NULL
                AND assistant_text_value IS NULL
                AND producing_model_call_id IS NULL
                AND assistant_tool_request_id IS NULL
                AND completed_turn_id IS NULL
            )
            OR
            (
                payload_kind = 'turn_failed'
                AND origin_accepted_input_id IS NULL
                AND failed_turn_id IS NOT NULL
                AND assistant_text_value IS NULL
                AND producing_model_call_id IS NULL
                AND assistant_tool_request_id IS NULL
                AND completed_turn_id IS NULL
            )
            OR
            (
                payload_kind = 'assistant_text'
                AND origin_accepted_input_id IS NULL
                AND failed_turn_id IS NULL
                AND assistant_text_value IS NOT NULL
                AND producing_model_call_id IS NOT NULL
                AND assistant_tool_request_id IS NULL
                AND completed_turn_id IS NULL
                AND assistant_text_value <> ''
            )
            OR
            (
                payload_kind = 'assistant_tool_use'
                AND origin_accepted_input_id IS NULL
                AND failed_turn_id IS NULL
                AND assistant_text_value IS NULL
                AND producing_model_call_id IS NOT NULL
                AND assistant_tool_request_id IS NOT NULL
                AND completed_turn_id IS NULL
            )
            OR
            (
                payload_kind = 'turn_completed'
                AND origin_accepted_input_id IS NULL
                AND failed_turn_id IS NULL
                AND assistant_text_value IS NULL
                AND producing_model_call_id IS NULL
                AND assistant_tool_request_id IS NULL
                AND completed_turn_id IS NOT NULL
            )
        ),
    -- ADR-0042 names the representation, while its reserved tool decisions
    -- intentionally withhold construction authority in this schema version.
    ADD CONSTRAINT semantic_transcript_entry_tool_use_unavailable
        CHECK (payload_kind <> 'assistant_tool_use'),
    ADD CONSTRAINT semantic_transcript_entry_turn_completed_once
        UNIQUE (completed_turn_id),
    ADD CONSTRAINT semantic_transcript_entry_producing_call_fk
        FOREIGN KEY (producing_model_call_id, source_session_id)
        REFERENCES model_call (model_call_id, session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    ADD CONSTRAINT semantic_transcript_entry_completed_turn_fk
        FOREIGN KEY (completed_turn_id, source_session_id)
        REFERENCES turn_lifecycle (turn_id, session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED;

ALTER TABLE turn_lifecycle
    ADD COLUMN recovery_model_call_id uuid,
    ADD COLUMN terminal_attempt_id uuid,
    ADD COLUMN terminal_model_call_id uuid;

ALTER TABLE turn_lifecycle
    DROP CONSTRAINT turn_lifecycle_active_phase_closed,
    DROP CONSTRAINT turn_lifecycle_terminal_disposition_closed,
    DROP CONSTRAINT turn_lifecycle_state_payload_shape;

ALTER TABLE turn_lifecycle
    ADD CONSTRAINT turn_lifecycle_active_phase_closed
        CHECK (
            active_phase_kind IS NULL
            OR active_phase_kind IN (
                'running',
                'awaiting_model_call_recovery'
            )
        ),
    ADD CONSTRAINT turn_lifecycle_terminal_disposition_closed
        CHECK (
            terminal_disposition_kind IS NULL
            OR terminal_disposition_kind IN (
                'failed',
                'completed',
                'refused'
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
                AND terminal_disposition_kind IN ('completed', 'refused')
                AND recovery_model_call_id IS NULL
                AND terminal_attempt_id IS NOT NULL
                AND terminal_model_call_id IS NOT NULL
            )
        ),
    ADD CONSTRAINT turn_lifecycle_recovery_call_fk
        FOREIGN KEY (recovery_model_call_id, turn_id, session_id)
        REFERENCES model_call (model_call_id, turn_id, session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    ADD CONSTRAINT turn_lifecycle_terminal_attempt_fk
        FOREIGN KEY (terminal_attempt_id, turn_id, session_id)
        REFERENCES turn_attempt (turn_attempt_id, turn_id, session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    ADD CONSTRAINT turn_lifecycle_terminal_call_fk
        FOREIGN KEY (terminal_model_call_id, turn_id, session_id)
        REFERENCES model_call (model_call_id, turn_id, session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED;

CREATE OR REPLACE FUNCTION reject_turn_lifecycle_invalid_change()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        IF NEW.state_kind <> 'queued' THEN
            RAISE EXCEPTION 'turn lifecycle must be inserted as queued'
                USING
                    ERRCODE = '23514',
                    CONSTRAINT = 'turn_lifecycle_inserted_queued';
        END IF;

        IF NEW.attempt_history_present THEN
            RAISE EXCEPTION 'turn lifecycle must be inserted without attempt history'
                USING ERRCODE = '23514';
        END IF;

        IF NEW.pinned_provider_model_identity_id IS NOT NULL THEN
            RAISE EXCEPTION 'queued turn lifecycle cannot begin with a provider target pin'
                USING ERRCODE = '23514';
        END IF;
        RETURN NEW;
    END IF;

    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'turn_lifecycle is not deletable'
            USING ERRCODE = '23514';
    END IF;

    IF ROW(
        OLD.turn_id,
        OLD.session_id,
        OLD.origin_accepted_input_id,
        OLD.acceptance_position
    ) IS DISTINCT FROM ROW(
        NEW.turn_id,
        NEW.session_id,
        NEW.origin_accepted_input_id,
        NEW.acceptance_position
    ) THEN
        RAISE EXCEPTION 'turn lifecycle identity, ownership, origin, and order are immutable'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.start_lineage_kind IS NOT NULL
       AND ROW(
           OLD.start_lineage_kind,
           OLD.immediate_predecessor_turn_id,
           OLD.starting_frontier_id
       ) IS DISTINCT FROM ROW(
           NEW.start_lineage_kind,
           NEW.immediate_predecessor_turn_id,
           NEW.starting_frontier_id
       )
    THEN
        RAISE EXCEPTION 'turn start is write-once'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.pinned_provider_model_identity_id IS NOT NULL
       AND NEW.pinned_provider_model_identity_id
           IS DISTINCT FROM OLD.pinned_provider_model_identity_id
    THEN
        RAISE EXCEPTION 'turn-level provider target pin is immutable'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.pinned_provider_model_identity_id IS NULL
       AND NEW.pinned_provider_model_identity_id IS NOT NULL
       AND (
            OLD.state_kind IS DISTINCT FROM 'active'
            OR NEW.state_kind IS DISTINCT FROM 'active'
            OR OLD.active_phase_kind IS DISTINCT FROM 'running'
            OR NEW.active_phase_kind IS DISTINCT FROM 'running'
            OR OLD.current_attempt_id IS NULL
            OR NEW.current_attempt_id IS DISTINCT FROM OLD.current_attempt_id
       )
    THEN
        RAISE EXCEPTION 'provider target can be pinned only for the current running attempt'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.state_kind = 'terminal' THEN
        RAISE EXCEPTION 'terminal turn lifecycle is immutable'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.attempt_history_present AND NOT NEW.attempt_history_present THEN
        RAISE EXCEPTION 'turn attempt history marker is write-once'
            USING ERRCODE = '23514';
    END IF;

    IF NOT (
        OLD.state_kind = NEW.state_kind
        OR (OLD.state_kind = 'queued' AND NEW.state_kind IN ('active', 'terminal'))
        OR (OLD.state_kind = 'active' AND NEW.state_kind = 'terminal')
    ) THEN
        RAISE EXCEPTION 'turn lifecycle transition is not monotonic'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.state_kind = 'active'
       AND OLD.active_phase_kind = 'awaiting_model_call_recovery'
       AND NEW.state_kind = 'active'
    THEN
        RAISE EXCEPTION 'model-call recovery wait cannot reopen as running'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.state_kind = 'active'
       AND OLD.active_phase_kind = 'running'
       AND NEW.state_kind = 'active'
       AND NEW.active_phase_kind = 'running'
       AND OLD.current_attempt_id IS DISTINCT FROM NEW.current_attempt_id
    THEN
        RAISE EXCEPTION 'running turn cannot replace its current attempt'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.state_kind = 'queued'
       AND NEW.state_kind = 'terminal'
       AND NEW.attempt_history_present
    THEN
        RAISE EXCEPTION 'a queued turn must terminalize without attempt history'
            USING
                ERRCODE = '23514',
                CONSTRAINT = 'turn_lifecycle_queued_failure_without_attempt';
    END IF;

    RETURN NEW;
END;
$$;

CREATE FUNCTION assert_model_call_final_state(
    checked_model_call_id uuid
)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    checked_turn_id uuid;
    checked_session_id uuid;
    checked_attempt_id uuid;
    checked_selection_kind text;
    checked_direct_id uuid;
    checked_alias_id uuid;
    checked_alias_selected_id uuid;
    checked_target_id uuid;
    checked_frontier_id uuid;
    checked_state text;
    checked_disposition text;
    origin_frozen_kind text;
    origin_direct_id uuid;
    origin_alias_id uuid;
    origin_alias_selected_id uuid;
    pinned_target_id uuid;
    attempt_state text;
    attempt_disposition text;
    turn_state text;
    active_phase text;
    current_attempt uuid;
    recovery_call uuid;
    terminal_attempt uuid;
    terminal_call uuid;
    terminal_disposition text;
    starting_frontier uuid;
BEGIN
    SELECT
        turn_id,
        session_id,
        turn_attempt_id,
        selection_kind,
        direct_model_selection_id,
        frozen_model_alias_id,
        frozen_alias_selected_direct_id,
        resolved_provider_model_identity_id,
        context_frontier_id,
        state_kind,
        terminal_disposition_kind
      INTO
        checked_turn_id,
        checked_session_id,
        checked_attempt_id,
        checked_selection_kind,
        checked_direct_id,
        checked_alias_id,
        checked_alias_selected_id,
        checked_target_id,
        checked_frontier_id,
        checked_state,
        checked_disposition
      FROM model_call
     WHERE model_call_id = checked_model_call_id;

    IF NOT FOUND THEN
        RETURN;
    END IF;

    WITH RECURSIVE configuration_origin AS (
        SELECT stored.*
          FROM queued_input_origin AS stored
         WHERE stored.turn_id = checked_turn_id
           AND stored.session_id = checked_session_id
        UNION
        SELECT source.*
          FROM configuration_origin AS current
          JOIN queued_input_origin AS source
            ON source.turn_id = current.source_configuration_turn_id
           AND source.session_id = current.session_id
    )
    SELECT
        origin.frozen_model_kind,
        origin.frozen_direct_model_selection_id,
        origin.frozen_model_alias_id,
        origin.frozen_alias_selected_direct_id,
        lifecycle.pinned_provider_model_identity_id,
        lifecycle.state_kind,
        lifecycle.active_phase_kind,
        lifecycle.current_attempt_id,
        lifecycle.recovery_model_call_id,
        lifecycle.terminal_attempt_id,
        lifecycle.terminal_model_call_id,
        lifecycle.terminal_disposition_kind,
        lifecycle.starting_frontier_id
      INTO
        origin_frozen_kind,
        origin_direct_id,
        origin_alias_id,
        origin_alias_selected_id,
        pinned_target_id,
        turn_state,
        active_phase,
        current_attempt,
        recovery_call,
        terminal_attempt,
        terminal_call,
        terminal_disposition,
        starting_frontier
      FROM turn_lifecycle AS lifecycle
      JOIN configuration_origin AS origin
        ON origin.session_id = lifecycle.session_id
       AND origin.source_configuration_turn_id IS NULL
     WHERE lifecycle.turn_id = checked_turn_id
       AND lifecycle.session_id = checked_session_id;

    IF NOT FOUND THEN
        RAISE EXCEPTION 'model call requires its exact owning turn'
            USING ERRCODE = '23503';
    END IF;

    IF ROW(
        checked_selection_kind,
        checked_direct_id,
        checked_alias_id,
        checked_alias_selected_id
    ) IS DISTINCT FROM ROW(
        origin_frozen_kind,
        origin_direct_id,
        origin_alias_id,
        origin_alias_selected_id
    ) THEN
        RAISE EXCEPTION 'model call selection differs from its frozen turn selection'
            USING ERRCODE = '23514';
    END IF;

    IF pinned_target_id IS DISTINCT FROM checked_target_id THEN
        RAISE EXCEPTION 'model call target differs from its independent turn-level pin'
            USING ERRCODE = '23514';
    END IF;

    -- The first execution slice admits one initial call and no steering. A
    -- later continuation slice must deliberately replace this exact binding.
    IF checked_frontier_id IS DISTINCT FROM starting_frontier THEN
        RAISE EXCEPTION 'initial model call must consume the exact starting frontier'
            USING ERRCODE = '23514';
    END IF;

    SELECT state_kind, end_disposition
      INTO attempt_state, attempt_disposition
      FROM turn_attempt
     WHERE turn_attempt_id = checked_attempt_id
       AND turn_id = checked_turn_id
       AND session_id = checked_session_id;

    IF NOT FOUND THEN
        RAISE EXCEPTION 'model call requires its exact owning attempt'
            USING ERRCODE = '23503';
    END IF;

    IF checked_state = 'prepared' THEN
        IF turn_state IS DISTINCT FROM 'active'
           OR active_phase IS DISTINCT FROM 'running'
           OR current_attempt IS DISTINCT FROM checked_attempt_id
           OR attempt_state IS DISTINCT FROM 'prepared'
        THEN
            RAISE EXCEPTION 'Prepared model call is not paired with its prepared attempt'
                USING ERRCODE = '23514';
        END IF;
    ELSIF checked_state IN ('in_flight', 'cancellation_requested') THEN
        IF turn_state IS DISTINCT FROM 'active'
           OR active_phase IS DISTINCT FROM 'running'
           OR current_attempt IS DISTINCT FROM checked_attempt_id
           OR attempt_state IS DISTINCT FROM 'running'
        THEN
            RAISE EXCEPTION 'issued model call is not paired with its running attempt'
                USING ERRCODE = '23514';
        END IF;
    ELSIF checked_disposition = 'ambiguous' THEN
        IF turn_state IS DISTINCT FROM 'active'
           OR active_phase IS DISTINCT FROM 'awaiting_model_call_recovery'
           OR current_attempt IS DISTINCT FROM checked_attempt_id
           OR recovery_call IS DISTINCT FROM checked_model_call_id
           OR attempt_state IS DISTINCT FROM 'ended'
           OR attempt_disposition NOT IN ('ambiguous', 'lost')
        THEN
            RAISE EXCEPTION 'Ambiguous model call lacks its exact durable recovery wait'
                USING ERRCODE = '23514';
        END IF;
    ELSIF checked_disposition = 'completed' THEN
        IF turn_state IS DISTINCT FROM 'terminal'
           OR terminal_disposition IS DISTINCT FROM 'completed'
           OR terminal_attempt IS DISTINCT FROM checked_attempt_id
           OR terminal_call IS DISTINCT FROM checked_model_call_id
           OR attempt_state IS DISTINCT FROM 'ended'
           OR (
                attempt_disposition IS DISTINCT FROM 'turn_completed'
                AND attempt_disposition IS DISTINCT FROM 'lost'
           )
        THEN
            RAISE EXCEPTION 'Completed model call lacks its exact terminal turn outcome'
                USING ERRCODE = '23514';
        END IF;
    ELSIF checked_disposition = 'refused' THEN
        IF turn_state IS DISTINCT FROM 'terminal'
           OR terminal_disposition IS DISTINCT FROM 'refused'
           OR terminal_attempt IS DISTINCT FROM checked_attempt_id
           OR terminal_call IS DISTINCT FROM checked_model_call_id
           OR attempt_state IS DISTINCT FROM 'ended'
           OR (
                attempt_disposition IS DISTINCT FROM 'turn_refused'
                AND attempt_disposition IS DISTINCT FROM 'lost'
           )
        THEN
            RAISE EXCEPTION 'Refused model call lacks its exact terminal turn outcome'
                USING ERRCODE = '23514';
        END IF;
    ELSE
        IF turn_state IS DISTINCT FROM 'terminal'
           OR terminal_disposition IS DISTINCT FROM 'failed'
           OR attempt_state IS DISTINCT FROM 'ended'
           OR attempt_disposition NOT IN ('known_failure', 'lost')
        THEN
            RAISE EXCEPTION 'failed physical call lacks its exact failed turn outcome'
                USING ERRCODE = '23514';
        END IF;
    END IF;
END;
$$;

CREATE FUNCTION require_model_call_final_state()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    PERFORM assert_model_call_final_state(
        CASE WHEN TG_OP = 'DELETE' THEN OLD.model_call_id ELSE NEW.model_call_id END
    );
    PERFORM assert_turn_lifecycle_final_state(
        CASE WHEN TG_OP = 'DELETE' THEN OLD.turn_id ELSE NEW.turn_id END
    );
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER model_call_requires_complete_final_state
AFTER INSERT OR UPDATE OR DELETE ON model_call
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_model_call_final_state();

CREATE OR REPLACE FUNCTION assert_turn_lifecycle_final_state(
    checked_turn_id uuid
)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    checked_session_id uuid;
    checked_origin_input_id uuid;
    checked_position numeric(20, 0);
    checked_attempt_history_present boolean;
    checked_state text;
    checked_lineage text;
    checked_predecessor uuid;
    checked_starting_frontier uuid;
    checked_terminal_frontier uuid;
    checked_active_phase text;
    checked_current_attempt uuid;
    checked_recovery_call uuid;
    checked_terminal_attempt uuid;
    checked_terminal_call uuid;
    checked_terminal_disposition text;
    attempt_count bigint;
    live_attempt_count bigint;
    exact_attempt_count bigint;
    contradictory_failed_attempt_count bigint;
    origin_entry_count bigint;
    origin_entry_id uuid;
    failure_entry_count bigint;
    failure_entry_id uuid;
    completion_entry_count bigint;
    completion_entry_id uuid;
    assistant_entry_count bigint;
    assistant_member_count bigint;
    origin_member_count bigint;
    origin_member_position numeric(20, 0);
    last_member_position numeric(20, 0);
    failure_member_count bigint;
    starting_member_count numeric(20, 0);
    terminal_member_count numeric(20, 0);
    predecessor_terminal_frontier uuid;
    predecessor_terminal_member_count numeric(20, 0);
    prefix_mismatch_count bigint;
    predecessor_state text;
    predecessor_position numeric(20, 0);
    expected_predecessor_position numeric(20, 0);
BEGIN
    SELECT
        session_id,
        origin_accepted_input_id,
        acceptance_position,
        attempt_history_present,
        state_kind,
        start_lineage_kind,
        immediate_predecessor_turn_id,
        starting_frontier_id,
        terminal_frontier_id,
        active_phase_kind,
        current_attempt_id,
        recovery_model_call_id,
        terminal_attempt_id,
        terminal_model_call_id,
        terminal_disposition_kind
      INTO
        checked_session_id,
        checked_origin_input_id,
        checked_position,
        checked_attempt_history_present,
        checked_state,
        checked_lineage,
        checked_predecessor,
        checked_starting_frontier,
        checked_terminal_frontier,
        checked_active_phase,
        checked_current_attempt,
        checked_recovery_call,
        checked_terminal_attempt,
        checked_terminal_call,
        checked_terminal_disposition
      FROM turn_lifecycle
     WHERE turn_id = checked_turn_id;

    IF NOT FOUND THEN
        RETURN;
    END IF;

    SELECT
        count(*),
        count(*) FILTER (WHERE state_kind <> 'ended'),
        count(*) FILTER (
            WHERE turn_attempt_id = COALESCE(
                checked_current_attempt,
                checked_terminal_attempt
            )
        ),
        count(*) FILTER (
            WHERE state_kind <> 'ended'
               OR end_disposition NOT IN ('known_failure', 'lost')
        )
      INTO
        attempt_count,
        live_attempt_count,
        exact_attempt_count,
        contradictory_failed_attempt_count
      FROM turn_attempt
     WHERE turn_id = checked_turn_id
       AND session_id = checked_session_id;

    IF checked_attempt_history_present IS DISTINCT FROM (attempt_count > 0) THEN
        RAISE EXCEPTION 'turn % attempt marker disagrees with durable attempts', checked_turn_id
            USING ERRCODE = '23514';
    END IF;

    SELECT count(*)
      INTO origin_entry_count
      FROM semantic_transcript_entry
     WHERE source_session_id = checked_session_id
       AND payload_kind = 'origin_accepted_input'
       AND origin_accepted_input_id = checked_origin_input_id;

    SELECT semantic_entry_id
      INTO origin_entry_id
      FROM semantic_transcript_entry
     WHERE source_session_id = checked_session_id
       AND payload_kind = 'origin_accepted_input'
       AND origin_accepted_input_id = checked_origin_input_id;

    SELECT count(*)
      INTO failure_entry_count
      FROM semantic_transcript_entry
     WHERE source_session_id = checked_session_id
       AND payload_kind = 'turn_failed'
       AND failed_turn_id = checked_turn_id;

    SELECT semantic_entry_id
      INTO failure_entry_id
      FROM semantic_transcript_entry
     WHERE source_session_id = checked_session_id
       AND payload_kind = 'turn_failed'
       AND failed_turn_id = checked_turn_id;

    SELECT count(*)
      INTO completion_entry_count
      FROM semantic_transcript_entry
     WHERE source_session_id = checked_session_id
       AND payload_kind = 'turn_completed'
       AND completed_turn_id = checked_turn_id;

    SELECT semantic_entry_id
      INTO completion_entry_id
      FROM semantic_transcript_entry
     WHERE source_session_id = checked_session_id
       AND payload_kind = 'turn_completed'
       AND completed_turn_id = checked_turn_id;

    IF checked_state = 'queued' THEN
        IF attempt_count <> 0
           OR origin_entry_count <> 0
           OR failure_entry_count <> 0
           OR completion_entry_count <> 0
        THEN
            RAISE EXCEPTION 'queued turn % carries started or terminal facts', checked_turn_id
                USING ERRCODE = '23514';
        END IF;
        RETURN;
    END IF;

    IF origin_entry_count <> 1 THEN
        RAISE EXCEPTION 'started turn % requires its exact origin entry', checked_turn_id
            USING ERRCODE = '23503';
    END IF;

    SELECT member_count
      INTO starting_member_count
      FROM context_frontier
     WHERE owning_session_id = checked_session_id
       AND context_frontier_id = checked_starting_frontier;

    SELECT max(member_position)
      INTO last_member_position
      FROM context_frontier_member
     WHERE owning_session_id = checked_session_id
       AND context_frontier_id = checked_starting_frontier;

    SELECT count(*), max(member_position)
      INTO origin_member_count, origin_member_position
      FROM context_frontier_member
     WHERE owning_session_id = checked_session_id
       AND context_frontier_id = checked_starting_frontier
       AND source_session_id = checked_session_id
       AND semantic_entry_id = origin_entry_id;

    IF origin_member_count <> 1
       OR origin_member_position IS DISTINCT FROM last_member_position
    THEN
        RAISE EXCEPTION 'turn % starting frontier does not end in its origin', checked_turn_id
            USING ERRCODE = '23503';
    END IF;

    IF checked_lineage = 'first_in_session' THEN
        IF starting_member_count IS DISTINCT FROM 1
           OR EXISTS (
            SELECT 1
              FROM turn_lifecycle AS earlier
             WHERE earlier.session_id = checked_session_id
               AND earlier.turn_id <> checked_turn_id
               AND earlier.acceptance_position < checked_position
        ) THEN
            RAISE EXCEPTION 'turn % has invalid first lineage', checked_turn_id
                USING ERRCODE = '23514';
        END IF;
    ELSE
        SELECT state_kind, acceptance_position, terminal_frontier_id
          INTO predecessor_state, predecessor_position, predecessor_terminal_frontier
          FROM turn_lifecycle
         WHERE turn_id = checked_predecessor
           AND session_id = checked_session_id;

        SELECT max(acceptance_position)
          INTO expected_predecessor_position
          FROM turn_lifecycle
         WHERE session_id = checked_session_id
           AND acceptance_position < checked_position;

        IF predecessor_state IS DISTINCT FROM 'terminal'
           OR predecessor_position IS DISTINCT FROM expected_predecessor_position
        THEN
            RAISE EXCEPTION 'turn % does not follow its immediate terminal predecessor', checked_turn_id
                USING ERRCODE = '23514';
        END IF;

        SELECT member_count
          INTO predecessor_terminal_member_count
          FROM context_frontier
         WHERE owning_session_id = checked_session_id
           AND context_frontier_id = predecessor_terminal_frontier;

        SELECT count(*)
          INTO prefix_mismatch_count
          FROM context_frontier_member AS predecessor_member
          LEFT JOIN context_frontier_member AS starting_member
            ON starting_member.owning_session_id = checked_session_id
           AND starting_member.context_frontier_id = checked_starting_frontier
           AND starting_member.member_position = predecessor_member.member_position
           AND starting_member.source_session_id = predecessor_member.source_session_id
           AND starting_member.semantic_entry_id = predecessor_member.semantic_entry_id
         WHERE predecessor_member.owning_session_id = checked_session_id
           AND predecessor_member.context_frontier_id = predecessor_terminal_frontier
           AND starting_member.member_position IS NULL;

        IF starting_member_count IS DISTINCT FROM predecessor_terminal_member_count + 1
           OR prefix_mismatch_count <> 0
        THEN
            RAISE EXCEPTION 'turn % starting frontier is not predecessor prefix plus origin', checked_turn_id
                USING ERRCODE = '23514';
        END IF;
    END IF;

    IF checked_state = 'active' THEN
        IF failure_entry_count <> 0 OR completion_entry_count <> 0 THEN
            RAISE EXCEPTION 'active turn % carries a terminal semantic marker', checked_turn_id
                USING ERRCODE = '23514';
        END IF;

        IF checked_active_phase = 'running' THEN
            IF live_attempt_count <> 1 OR exact_attempt_count <> 1 THEN
                RAISE EXCEPTION 'running turn % requires its exact live attempt', checked_turn_id
                    USING ERRCODE = '23514';
            END IF;
        ELSE
            IF live_attempt_count <> 0
               OR exact_attempt_count <> 1
               OR NOT EXISTS (
                    SELECT 1
                      FROM turn_attempt
                     WHERE turn_attempt_id = checked_current_attempt
                       AND turn_id = checked_turn_id
                       AND session_id = checked_session_id
                       AND state_kind = 'ended'
                       AND end_disposition IN ('ambiguous', 'lost')
               )
               OR NOT EXISTS (
                    SELECT 1
                      FROM model_call
                     WHERE model_call_id = checked_recovery_call
                       AND turn_attempt_id = checked_current_attempt
                       AND turn_id = checked_turn_id
                       AND session_id = checked_session_id
                       AND state_kind = 'terminal'
                       AND terminal_disposition_kind = 'ambiguous'
               )
            THEN
                RAISE EXCEPTION 'turn % has an incomplete model-call recovery wait', checked_turn_id
                    USING ERRCODE = '23514';
            END IF;
        END IF;
        RETURN;
    END IF;

    IF live_attempt_count <> 0 THEN
        RAISE EXCEPTION 'terminal turn % retains a live attempt', checked_turn_id
            USING ERRCODE = '23514';
    END IF;

    SELECT member_count
      INTO terminal_member_count
      FROM context_frontier
     WHERE owning_session_id = checked_session_id
       AND context_frontier_id = checked_terminal_frontier;

    SELECT count(*)
      INTO prefix_mismatch_count
      FROM context_frontier_member AS starting_member
      LEFT JOIN context_frontier_member AS terminal_member
        ON terminal_member.owning_session_id = checked_session_id
       AND terminal_member.context_frontier_id = checked_terminal_frontier
       AND terminal_member.member_position = starting_member.member_position
       AND terminal_member.source_session_id = starting_member.source_session_id
       AND terminal_member.semantic_entry_id = starting_member.semantic_entry_id
     WHERE starting_member.owning_session_id = checked_session_id
       AND starting_member.context_frontier_id = checked_starting_frontier
       AND terminal_member.member_position IS NULL;

    IF checked_terminal_disposition = 'failed' THEN
        IF contradictory_failed_attempt_count <> 0 THEN
            RAISE EXCEPTION
                'failed terminal turn % permits only known_failure or lost ended attempts',
                checked_turn_id
                USING ERRCODE = '23514';
        END IF;

        IF failure_entry_count <> 1
           OR completion_entry_count <> 0
           OR EXISTS (
                SELECT 1
                  FROM model_call
                 WHERE turn_id = checked_turn_id
                   AND session_id = checked_session_id
                   AND (
                        state_kind <> 'terminal'
                        OR terminal_disposition_kind NOT IN (
                            'known_failed',
                            'cancelled'
                        )
                   )
           )
        THEN
            RAISE EXCEPTION 'failed turn % has contradictory terminal facts', checked_turn_id
                USING ERRCODE = '23514';
        END IF;

        SELECT count(*)
          INTO failure_member_count
          FROM context_frontier_member
         WHERE owning_session_id = checked_session_id
           AND context_frontier_id = checked_terminal_frontier
           AND source_session_id = checked_session_id
           AND semantic_entry_id = failure_entry_id;

        IF terminal_member_count IS DISTINCT FROM starting_member_count + 1
           OR prefix_mismatch_count <> 0
           OR failure_member_count <> 1
           OR NOT EXISTS (
                SELECT 1
                  FROM context_frontier_member
                 WHERE owning_session_id = checked_session_id
                   AND context_frontier_id = checked_terminal_frontier
                   AND member_position = terminal_member_count
                   AND source_session_id = checked_session_id
                   AND semantic_entry_id = failure_entry_id
           )
        THEN
            RAISE EXCEPTION 'failed turn % terminal frontier is not prefix plus failure', checked_turn_id
                USING ERRCODE = '23514';
        END IF;
    ELSIF checked_terminal_disposition = 'refused' THEN
        IF failure_entry_count <> 0
           OR completion_entry_count <> 0
           OR checked_terminal_frontier = checked_starting_frontier
           OR terminal_member_count IS DISTINCT FROM starting_member_count
           OR prefix_mismatch_count <> 0
           OR NOT EXISTS (
                SELECT 1
                  FROM turn_attempt
                 WHERE turn_attempt_id = checked_terminal_attempt
                   AND end_disposition IN ('turn_refused', 'lost')
           )
           OR NOT EXISTS (
                SELECT 1
                  FROM model_call
                 WHERE model_call_id = checked_terminal_call
                   AND turn_attempt_id = checked_terminal_attempt
                   AND terminal_disposition_kind = 'refused'
           )
        THEN
            RAISE EXCEPTION 'refused turn % lacks its exact equal-content boundary', checked_turn_id
                USING ERRCODE = '23514';
        END IF;
    ELSE
        SELECT count(*)
          INTO assistant_entry_count
          FROM semantic_transcript_entry
         WHERE source_session_id = checked_session_id
           AND payload_kind = 'assistant_text'
           AND producing_model_call_id = checked_terminal_call;

        SELECT count(*)
          INTO assistant_member_count
          FROM context_frontier_member AS member
          JOIN semantic_transcript_entry AS entry
            ON entry.source_session_id = member.source_session_id
           AND entry.semantic_entry_id = member.semantic_entry_id
         WHERE member.owning_session_id = checked_session_id
           AND member.context_frontier_id = checked_terminal_frontier
           AND member.member_position > starting_member_count
           AND member.member_position < terminal_member_count
           AND entry.payload_kind = 'assistant_text'
           AND entry.producing_model_call_id = checked_terminal_call;

        IF failure_entry_count <> 0
           OR completion_entry_count <> 1
           OR terminal_member_count
                IS DISTINCT FROM starting_member_count + assistant_entry_count + 1
           OR prefix_mismatch_count <> 0
           OR assistant_member_count <> assistant_entry_count
           OR NOT EXISTS (
                SELECT 1
                  FROM context_frontier_member
                 WHERE owning_session_id = checked_session_id
                   AND context_frontier_id = checked_terminal_frontier
                   AND member_position = terminal_member_count
                   AND source_session_id = checked_session_id
                   AND semantic_entry_id = completion_entry_id
           )
           OR NOT EXISTS (
                SELECT 1
                  FROM turn_attempt
                 WHERE turn_attempt_id = checked_terminal_attempt
                   AND end_disposition IN ('turn_completed', 'lost')
           )
           OR NOT EXISTS (
                SELECT 1
                  FROM model_call
                 WHERE model_call_id = checked_terminal_call
                   AND turn_attempt_id = checked_terminal_attempt
                   AND terminal_disposition_kind = 'completed'
           )
        THEN
            RAISE EXCEPTION 'completed turn % lacks its atomic ordered response boundary', checked_turn_id
                USING ERRCODE = '23514';
        END IF;
    END IF;
END;
$$;

CREATE OR REPLACE FUNCTION require_semantic_entry_turn_state()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    checked_payload_kind text;
    checked_source_session_id uuid;
    checked_origin_input_id uuid;
    checked_failed_turn_id uuid;
    checked_producing_call_id uuid;
    checked_completed_turn_id uuid;
    checked_turn_id uuid;
BEGIN
    IF TG_OP = 'DELETE' THEN
        checked_payload_kind := OLD.payload_kind;
        checked_source_session_id := OLD.source_session_id;
        checked_origin_input_id := OLD.origin_accepted_input_id;
        checked_failed_turn_id := OLD.failed_turn_id;
        checked_producing_call_id := OLD.producing_model_call_id;
        checked_completed_turn_id := OLD.completed_turn_id;
    ELSE
        checked_payload_kind := NEW.payload_kind;
        checked_source_session_id := NEW.source_session_id;
        checked_origin_input_id := NEW.origin_accepted_input_id;
        checked_failed_turn_id := NEW.failed_turn_id;
        checked_producing_call_id := NEW.producing_model_call_id;
        checked_completed_turn_id := NEW.completed_turn_id;
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
                RAISE EXCEPTION 'semantic origin input % is not a turn origin', checked_origin_input_id
                    USING
                        ERRCODE = '23514',
                        CONSTRAINT = 'semantic_transcript_entry_origin_disposition';
            END IF;
        WHEN 'turn_failed' THEN
            checked_turn_id := checked_failed_turn_id;
        WHEN 'turn_completed' THEN
            checked_turn_id := checked_completed_turn_id;
        WHEN 'assistant_text' THEN
            SELECT turn_id
              INTO checked_turn_id
              FROM model_call
             WHERE model_call_id = checked_producing_call_id
               AND state_kind = 'terminal'
               AND terminal_disposition_kind = 'completed';

            IF NOT FOUND THEN
                RAISE EXCEPTION 'assistant text requires its outcome-authoritative completed call'
                    USING ERRCODE = '23514';
            END IF;
        ELSE
            RAISE EXCEPTION 'semantic payload kind % lacks construction authority', checked_payload_kind
                USING ERRCODE = '23514';
    END CASE;

    PERFORM assert_turn_lifecycle_final_state(checked_turn_id);
    IF checked_producing_call_id IS NOT NULL THEN
        PERFORM assert_model_call_final_state(checked_producing_call_id);
    END IF;
    RETURN NULL;
END;
$$;

-- ADR-0040 typed records for the client-visible checkpoints committed by this
-- slice. They are persistence projections, not an ADR-0019 wire schema.
ALTER TABLE outbox_event
    DROP CONSTRAINT outbox_event_kind_closed,
    DROP CONSTRAINT outbox_event_storage_version_supported;

ALTER TABLE outbox_event
    ADD CONSTRAINT outbox_event_kind_closed
        CHECK (
            event_kind IN (
                'session_created',
                'turn_failed',
                'model_call_transition',
                'turn_completed',
                'turn_refused'
            )
        ),
    ADD CONSTRAINT outbox_event_storage_version_supported
        CHECK (storage_version = 1);

CREATE TABLE model_call_transition_outbox_event (
    event_sequence numeric(20, 0) PRIMARY KEY,
    event_kind text NOT NULL,
    storage_version smallint NOT NULL,
    session_id uuid NOT NULL,
    model_call_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    call_state_kind text NOT NULL,
    terminal_disposition_kind text,

    CONSTRAINT model_call_transition_outbox_kind_closed
        CHECK (event_kind = 'model_call_transition'),
    CONSTRAINT model_call_transition_outbox_version_supported
        CHECK (storage_version = 1),
    CONSTRAINT model_call_transition_outbox_state_closed
        CHECK (call_state_kind IN ('prepared', 'in_flight', 'terminal')),
    CONSTRAINT model_call_transition_outbox_state_shape
        CHECK (
            (
                call_state_kind <> 'terminal'
                AND terminal_disposition_kind IS NULL
            )
            OR
            (
                call_state_kind = 'terminal'
                AND terminal_disposition_kind IN (
                    'completed',
                    'known_failed',
                    'refused',
                    'cancelled',
                    'ambiguous'
                )
            )
        ),
    CONSTRAINT model_call_transition_outbox_once
        UNIQUE (model_call_id, call_state_kind),
    CONSTRAINT model_call_transition_outbox_header_fk
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
    CONSTRAINT model_call_transition_outbox_call_fk
        FOREIGN KEY (model_call_id, turn_id, session_id)
        REFERENCES model_call (model_call_id, turn_id, session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE TABLE turn_completed_outbox_event (
    event_sequence numeric(20, 0) PRIMARY KEY,
    event_kind text NOT NULL,
    storage_version smallint NOT NULL,
    session_id uuid NOT NULL,
    turn_id uuid NOT NULL UNIQUE,
    model_call_id uuid NOT NULL UNIQUE,
    completion_entry_id uuid NOT NULL UNIQUE,
    terminal_frontier_id uuid NOT NULL UNIQUE,

    CONSTRAINT turn_completed_outbox_kind_closed
        CHECK (event_kind = 'turn_completed'),
    CONSTRAINT turn_completed_outbox_version_supported
        CHECK (storage_version = 1),
    CONSTRAINT turn_completed_outbox_header_fk
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
    CONSTRAINT turn_completed_outbox_turn_fk
        FOREIGN KEY (turn_id, session_id)
        REFERENCES turn_lifecycle (turn_id, session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT turn_completed_outbox_call_fk
        FOREIGN KEY (model_call_id, turn_id, session_id)
        REFERENCES model_call (model_call_id, turn_id, session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT turn_completed_outbox_entry_fk
        FOREIGN KEY (session_id, completion_entry_id)
        REFERENCES semantic_transcript_entry (source_session_id, semantic_entry_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT turn_completed_outbox_frontier_fk
        FOREIGN KEY (session_id, terminal_frontier_id)
        REFERENCES context_frontier (owning_session_id, context_frontier_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE TABLE turn_refused_outbox_event (
    event_sequence numeric(20, 0) PRIMARY KEY,
    event_kind text NOT NULL,
    storage_version smallint NOT NULL,
    session_id uuid NOT NULL,
    turn_id uuid NOT NULL UNIQUE,
    model_call_id uuid NOT NULL UNIQUE,
    terminal_frontier_id uuid NOT NULL UNIQUE,

    CONSTRAINT turn_refused_outbox_kind_closed
        CHECK (event_kind = 'turn_refused'),
    CONSTRAINT turn_refused_outbox_version_supported
        CHECK (storage_version = 1),
    CONSTRAINT turn_refused_outbox_header_fk
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
    CONSTRAINT turn_refused_outbox_turn_fk
        FOREIGN KEY (turn_id, session_id)
        REFERENCES turn_lifecycle (turn_id, session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT turn_refused_outbox_call_fk
        FOREIGN KEY (model_call_id, turn_id, session_id)
        REFERENCES model_call (model_call_id, turn_id, session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT turn_refused_outbox_frontier_fk
        FOREIGN KEY (session_id, terminal_frontier_id)
        REFERENCES context_frontier (owning_session_id, context_frontier_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

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

CREATE TRIGGER model_call_transition_outbox_event_is_append_only
BEFORE UPDATE OR DELETE ON model_call_transition_outbox_event
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER turn_completed_outbox_event_is_append_only
BEFORE UPDATE OR DELETE ON turn_completed_outbox_event
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER turn_refused_outbox_event_is_append_only
BEFORE UPDATE OR DELETE ON turn_refused_outbox_event
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER model_call_transition_outbox_event_cannot_be_truncated
BEFORE TRUNCATE ON model_call_transition_outbox_event
FOR EACH STATEMENT
EXECUTE FUNCTION reject_outbox_table_truncate();

CREATE TRIGGER turn_completed_outbox_event_cannot_be_truncated
BEFORE TRUNCATE ON turn_completed_outbox_event
FOR EACH STATEMENT
EXECUTE FUNCTION reject_outbox_table_truncate();

CREATE TRIGGER turn_refused_outbox_event_cannot_be_truncated
BEFORE TRUNCATE ON turn_refused_outbox_event
FOR EACH STATEMENT
EXECUTE FUNCTION reject_outbox_table_truncate();
