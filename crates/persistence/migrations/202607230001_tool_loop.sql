-- Durable in-turn tool rounds, approval commands, and fenced tool attempts.
--
-- Legacy tool-free turns retain the prior commit-boundary assertions. Tool
-- rounds add their own normalized authority records and are selected by the
-- same deferred validators only when a turn actually contains a tool round.

-- The dangerous blanket posture is versioned session configuration. Existing
-- rows are the fail-closed version-one value; new CreateSession and
-- ReplaceSessionDefaults writes use kind-scoped storage version two.
ALTER TABLE session_defaults_version
    ADD COLUMN dangerous_tool_auto_approval text NOT NULL DEFAULT 'disabled';

ALTER TABLE session_defaults_version
    ADD CONSTRAINT session_defaults_version_tool_auto_approval_closed
        CHECK (dangerous_tool_auto_approval IN ('disabled', 'approve_all'));

ALTER TABLE create_session_command
    ADD COLUMN dangerous_tool_auto_approval text NOT NULL DEFAULT 'disabled',
    DROP CONSTRAINT create_session_command_storage_version_supported;

ALTER TABLE create_session_command
    ADD CONSTRAINT create_session_command_storage_version_supported
        CHECK (storage_version IN (1, 2)),
    ADD CONSTRAINT create_session_command_tool_auto_approval_closed
        CHECK (dangerous_tool_auto_approval IN ('disabled', 'approve_all')),
    ADD CONSTRAINT create_session_command_v1_tool_auto_approval
        CHECK (
            storage_version <> 1
            OR dangerous_tool_auto_approval = 'disabled'
        );

ALTER TABLE replace_session_defaults_command
    ADD COLUMN dangerous_tool_auto_approval text NOT NULL DEFAULT 'disabled',
    DROP CONSTRAINT replace_session_defaults_command_storage_version_supported;

ALTER TABLE replace_session_defaults_command
    ADD CONSTRAINT replace_session_defaults_command_storage_version_supported
        CHECK (storage_version IN (1, 2)),
    ADD CONSTRAINT replace_session_defaults_command_tool_auto_approval_closed
        CHECK (dangerous_tool_auto_approval IN ('disabled', 'approve_all')),
    ADD CONSTRAINT replace_session_defaults_command_v1_tool_auto_approval
        CHECK (
            storage_version <> 1
            OR dangerous_tool_auto_approval = 'disabled'
        );

ALTER TABLE create_session_command
    DROP CONSTRAINT create_session_command_initial_defaults_fk;

ALTER TABLE replace_session_defaults_command
    DROP CONSTRAINT replace_session_defaults_command_applied_defaults_fk;

ALTER TABLE session_defaults_version
    DROP CONSTRAINT session_defaults_version_selection_key;

ALTER TABLE session_defaults_version
    ADD CONSTRAINT session_defaults_version_selection_key
        UNIQUE (
            session_id,
            version,
            model_selection_kind,
            model_selection_reference,
            dangerous_tool_auto_approval
        );

ALTER TABLE create_session_command
    ADD CONSTRAINT create_session_command_initial_defaults_fk
        FOREIGN KEY (
            created_session_id,
            initial_defaults_version,
            model_selection_kind,
            model_selection_reference,
            dangerous_tool_auto_approval
        )
        REFERENCES session_defaults_version (
            session_id,
            version,
            model_selection_kind,
            model_selection_reference,
            dangerous_tool_auto_approval
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT;

ALTER TABLE replace_session_defaults_command
    ADD CONSTRAINT replace_session_defaults_command_applied_defaults_fk
        FOREIGN KEY (
            result_session_id,
            result_installed_version,
            model_selection_kind,
            model_selection_reference,
            dangerous_tool_auto_approval
        )
        REFERENCES session_defaults_version (
            session_id,
            version,
            model_selection_kind,
            model_selection_reference,
            dangerous_tool_auto_approval
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED;

ALTER TABLE queued_input_origin
    ADD COLUMN dangerous_tool_auto_approval text;

ALTER TABLE queued_input_origin
    DISABLE TRIGGER queued_input_origin_is_append_only;

UPDATE queued_input_origin
   SET dangerous_tool_auto_approval = 'disabled'
 WHERE source_configuration_turn_id IS NULL;

ALTER TABLE queued_input_origin
    ENABLE TRIGGER queued_input_origin_is_append_only;

ALTER TABLE queued_input_origin
    DROP CONSTRAINT queued_input_origin_configuration_provenance_shape;

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
                AND dangerous_tool_auto_approval IS NOT NULL
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
                AND dangerous_tool_auto_approval IS NULL
            )
        ),
    ADD CONSTRAINT queued_input_origin_tool_auto_approval_closed
        CHECK (
            source_configuration_turn_id IS NOT NULL
            OR dangerous_tool_auto_approval IN ('disabled', 'approve_all')
        );

CREATE FUNCTION default_v1_queued_tool_auto_approval()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.source_configuration_turn_id IS NULL
       AND NEW.dangerous_tool_auto_approval IS NULL
    THEN
        NEW.dangerous_tool_auto_approval := 'disabled';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER queued_input_origin_defaults_v1_tool_auto_approval
BEFORE INSERT ON queued_input_origin
FOR EACH ROW
EXECUTE FUNCTION default_v1_queued_tool_auto_approval();

ALTER TABLE durable_command
    DROP CONSTRAINT durable_command_kind_closed,
    DROP CONSTRAINT durable_command_storage_version_supported;

ALTER TABLE durable_command
    ADD CONSTRAINT durable_command_kind_closed
        CHECK (
            command_kind IN (
                'create_session',
                'replace_session_defaults',
                'submit_input',
                'decide_tool_request'
            )
        ),
    ADD CONSTRAINT durable_command_storage_version_supported
        CHECK (
            (
                command_kind IN (
                    'create_session',
                    'replace_session_defaults'
                )
                AND storage_version IN (1, 2)
            )
            OR (
                command_kind IN (
                    'submit_input',
                    'decide_tool_request'
                )
                AND storage_version = 1
            )
        );

CREATE TABLE decide_tool_request_command (
    command_id uuid PRIMARY KEY,
    command_kind text NOT NULL,
    storage_version smallint NOT NULL,
    request_id uuid NOT NULL,
    decision_kind text NOT NULL,
    denial_reason text,
    result_kind text NOT NULL,
    rejection_kind text,
    result_earliest_undecided_request_id uuid,

    CONSTRAINT decide_tool_request_command_kind_closed
        CHECK (command_kind = 'decide_tool_request'),
    CONSTRAINT decide_tool_request_command_storage_version_supported
        CHECK (storage_version = 1),
    CONSTRAINT decide_tool_request_command_decision_closed
        CHECK (decision_kind IN ('approve', 'deny')),
    CONSTRAINT decide_tool_request_command_decision_shape
        CHECK (
            (
                decision_kind = 'approve'
                AND denial_reason IS NULL
            )
            OR (
                decision_kind = 'deny'
                AND (
                    denial_reason IS NULL
                    OR (
                        octet_length(denial_reason) BETWEEN 1 AND 1024
                        AND denial_reason = btrim(denial_reason)
                    )
                )
            )
        ),
    CONSTRAINT decide_tool_request_command_result_closed
        CHECK (result_kind IN ('applied', 'rejected')),
    CONSTRAINT decide_tool_request_command_rejection_closed
        CHECK (
            rejection_kind IS NULL
            OR rejection_kind IN (
                'request_not_found',
                'already_resolved',
                'not_earliest_undecided'
            )
        ),
    CONSTRAINT decide_tool_request_command_result_shape
        CHECK (
            (
                result_kind = 'applied'
                AND rejection_kind IS NULL
                AND result_earliest_undecided_request_id IS NULL
            )
            OR (
                result_kind = 'rejected'
                AND rejection_kind IN (
                    'request_not_found',
                    'already_resolved'
                )
                AND result_earliest_undecided_request_id IS NULL
            )
            OR (
                result_kind = 'rejected'
                AND rejection_kind = 'not_earliest_undecided'
                AND result_earliest_undecided_request_id IS NOT NULL
                AND result_earliest_undecided_request_id <> request_id
            )
        ),
    CONSTRAINT decide_tool_request_command_registry_fk
        FOREIGN KEY (command_id, command_kind, storage_version)
        REFERENCES durable_command (
            command_id,
            command_kind,
            storage_version
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE TRIGGER decide_tool_request_command_is_append_only
BEFORE UPDATE OR DELETE ON decide_tool_request_command
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
        WHEN 'decide_tool_request' THEN
            SELECT count(*)
              INTO matching_records
              FROM decide_tool_request_command
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

-- One normalized round authority names the response boundary. Logical request
-- content and all later approval/execution evidence reference this record.
CREATE TABLE tool_round (
    producing_model_call_id uuid PRIMARY KEY,
    session_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    boundary_kind text NOT NULL,
    boundary_frontier_id uuid NOT NULL,
    response_part_count numeric(20, 0) NOT NULL,
    request_count numeric(20, 0) NOT NULL,

    CONSTRAINT tool_round_boundary_kind_closed
        CHECK (boundary_kind IN ('continuing', 'closed_by_turn_end')),
    CONSTRAINT tool_round_counts_bounded
        CHECK (
            response_part_count BETWEEN 1 AND 4294967295
            AND request_count BETWEEN 1 AND response_part_count
        ),
    CONSTRAINT tool_round_call_correlation_key
        UNIQUE (producing_model_call_id, turn_id, session_id),
    CONSTRAINT tool_round_call_fk
        FOREIGN KEY (producing_model_call_id, turn_id, session_id)
        REFERENCES model_call (model_call_id, turn_id, session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT tool_round_frontier_fk
        FOREIGN KEY (session_id, boundary_frontier_id)
        REFERENCES context_frontier (owning_session_id, context_frontier_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE TABLE tool_request (
    request_id uuid PRIMARY KEY,
    session_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    producing_model_call_id uuid NOT NULL,
    request_ordinal numeric(10, 0) NOT NULL,
    tool_name text NOT NULL,
    arguments_kind text NOT NULL,
    arguments_text text NOT NULL,

    CONSTRAINT tool_request_ordinal_u32
        CHECK (request_ordinal BETWEEN 0 AND 4294967295),
    CONSTRAINT tool_request_name_shape
        CHECK (
            octet_length(tool_name) BETWEEN 1 AND 64
            AND tool_name ~ '^[A-Za-z0-9_-]+$'
        ),
    CONSTRAINT tool_request_arguments_kind_closed
        CHECK (arguments_kind IN ('json', 'undecodable')),
    CONSTRAINT tool_request_arguments_bounded
        CHECK (octet_length(arguments_text) <= 1048576),
    CONSTRAINT tool_request_call_ordinal_once
        UNIQUE (producing_model_call_id, request_ordinal),
    CONSTRAINT tool_request_correlation_key
        UNIQUE (request_id, producing_model_call_id, session_id),
    CONSTRAINT tool_request_turn_correlation_key
        UNIQUE (request_id, turn_id, session_id),
    CONSTRAINT tool_request_round_fk
        FOREIGN KEY (producing_model_call_id, turn_id, session_id)
        REFERENCES tool_round (
            producing_model_call_id,
            turn_id,
            session_id
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE TABLE tool_approval_decision (
    request_id uuid PRIMARY KEY,
    decision_kind text NOT NULL,
    decision_source text NOT NULL,
    denial_reason text,
    owner_command_id uuid UNIQUE,

    CONSTRAINT tool_approval_decision_kind_closed
        CHECK (decision_kind IN ('approve', 'deny')),
    CONSTRAINT tool_approval_decision_source_closed
        CHECK (
            decision_source IN (
                'owner_command',
                'policy_auto',
                'session_blanket'
            )
        ),
    CONSTRAINT tool_approval_decision_shape
        CHECK (
            (
                decision_kind = 'approve'
                AND denial_reason IS NULL
            )
            OR (
                decision_kind = 'deny'
                AND decision_source = 'owner_command'
                AND (
                    denial_reason IS NULL
                    OR (
                        octet_length(denial_reason) BETWEEN 1 AND 1024
                        AND denial_reason = btrim(denial_reason)
                    )
                )
            )
        ),
    CONSTRAINT tool_approval_decision_source_shape
        CHECK (
            (
                decision_source = 'owner_command'
                AND owner_command_id IS NOT NULL
            )
            OR (
                decision_source IN ('policy_auto', 'session_blanket')
                AND decision_kind = 'approve'
                AND owner_command_id IS NULL
            )
        ),
    CONSTRAINT tool_approval_decision_request_fk
        FOREIGN KEY (request_id)
        REFERENCES tool_request (request_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CONSTRAINT tool_approval_decision_owner_command_fk
        FOREIGN KEY (owner_command_id)
        REFERENCES decide_tool_request_command (command_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE TABLE tool_attempt (
    attempt_id uuid PRIMARY KEY,
    request_id uuid NOT NULL UNIQUE,
    session_id uuid NOT NULL,
    turn_id uuid NOT NULL,
    issuing_turn_attempt_id uuid NOT NULL,
    effect_class text NOT NULL,
    dispatch_generation numeric(20, 0) NOT NULL,
    state_kind text NOT NULL,
    terminal_disposition_kind text,
    result_content_kind text,
    result_text text,
    error_kind text,
    error_detail text,

    CONSTRAINT tool_attempt_effect_class_closed
        CHECK (effect_class IN ('effect_free', 'external_effect')),
    CONSTRAINT tool_attempt_generation_v1
        CHECK (dispatch_generation = 1),
    CONSTRAINT tool_attempt_state_closed
        CHECK (state_kind IN ('prepared', 'in_flight', 'terminal')),
    CONSTRAINT tool_attempt_disposition_closed
        CHECK (
            terminal_disposition_kind IS NULL
            OR terminal_disposition_kind IN (
                'completed',
                'known_failed',
                'ambiguous'
            )
        ),
    CONSTRAINT tool_attempt_result_kind_closed
        CHECK (
            result_content_kind IS NULL
            OR result_content_kind = 'text'
        ),
    CONSTRAINT tool_attempt_error_kind_closed
        CHECK (
            error_kind IS NULL
            OR error_kind IN (
                'unknown_tool',
                'invalid_arguments',
                'execution_failed',
                'result_too_large',
                'crash_lost'
            )
        ),
    CONSTRAINT tool_attempt_error_detail_bounded
        CHECK (
            error_detail IS NULL
            OR (
                octet_length(error_detail) BETWEEN 1 AND 4096
                AND error_detail = btrim(error_detail)
            )
        ),
    CONSTRAINT tool_attempt_state_payload_shape
        CHECK (
            (
                state_kind IN ('prepared', 'in_flight')
                AND terminal_disposition_kind IS NULL
                AND result_content_kind IS NULL
                AND result_text IS NULL
                AND error_kind IS NULL
                AND error_detail IS NULL
            )
            OR (
                state_kind = 'terminal'
                AND terminal_disposition_kind = 'completed'
                AND result_content_kind = 'text'
                AND result_text IS NOT NULL
                AND octet_length(result_text) <= 1048576
                AND error_kind IS NULL
                AND error_detail IS NULL
            )
            OR (
                state_kind = 'terminal'
                AND terminal_disposition_kind = 'known_failed'
                AND result_content_kind IS NULL
                AND result_text IS NULL
                AND error_kind IS NOT NULL
            )
            OR (
                state_kind = 'terminal'
                AND terminal_disposition_kind = 'ambiguous'
                AND effect_class = 'external_effect'
                AND result_content_kind IS NULL
                AND result_text IS NULL
                AND error_kind IS NULL
                AND error_detail IS NULL
            )
        ),
    CONSTRAINT tool_attempt_correlation_key
        UNIQUE (
            attempt_id,
            request_id,
            issuing_turn_attempt_id,
            dispatch_generation
        ),
    CONSTRAINT tool_attempt_turn_correlation_key
        UNIQUE (attempt_id, turn_id, session_id),
    CONSTRAINT tool_attempt_request_fk
        FOREIGN KEY (request_id, turn_id, session_id)
        REFERENCES tool_request (request_id, turn_id, session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT tool_attempt_issuing_turn_attempt_fk
        FOREIGN KEY (issuing_turn_attempt_id, turn_id, session_id)
        REFERENCES turn_attempt (turn_attempt_id, turn_id, session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE UNIQUE INDEX tool_attempt_one_live_per_turn
    ON tool_attempt (turn_id)
    WHERE state_kind <> 'terminal';

CREATE TRIGGER tool_round_is_append_only
BEFORE UPDATE OR DELETE ON tool_round
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER tool_request_is_append_only
BEFORE UPDATE OR DELETE ON tool_request
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER tool_approval_decision_is_append_only
BEFORE UPDATE OR DELETE ON tool_approval_decision
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE FUNCTION reject_tool_attempt_invalid_change()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        IF NEW.state_kind <> 'prepared' THEN
            RAISE EXCEPTION 'tool attempt must be inserted as Prepared'
                USING
                    ERRCODE = '23514',
                    CONSTRAINT = 'tool_attempt_inserted_prepared';
        END IF;
        RETURN NEW;
    END IF;

    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'tool_attempt is not deletable'
            USING ERRCODE = '23514';
    END IF;

    IF ROW(
        OLD.attempt_id,
        OLD.request_id,
        OLD.session_id,
        OLD.turn_id,
        OLD.issuing_turn_attempt_id,
        OLD.effect_class,
        OLD.dispatch_generation
    ) IS DISTINCT FROM ROW(
        NEW.attempt_id,
        NEW.request_id,
        NEW.session_id,
        NEW.turn_id,
        NEW.issuing_turn_attempt_id,
        NEW.effect_class,
        NEW.dispatch_generation
    ) THEN
        RAISE EXCEPTION 'tool attempt authorization facts are immutable'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.state_kind = 'terminal' THEN
        RAISE EXCEPTION 'terminal tool attempt is immutable'
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
            AND NEW.state_kind = 'terminal'
        )
    ) THEN
        RAISE EXCEPTION 'tool attempt transition is not monotonic'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.state_kind = 'prepared'
       AND NEW.state_kind = 'terminal'
       AND (
            NEW.terminal_disposition_kind <> 'known_failed'
            OR NEW.error_kind NOT IN (
                'unknown_tool',
                'invalid_arguments',
                'crash_lost'
            )
       )
    THEN
        RAISE EXCEPTION 'unsent tool attempt has impossible terminal evidence'
            USING ERRCODE = '23514';
    END IF;

    RETURN NEW;
END;
$$;

CREATE TRIGGER tool_attempt_changes_are_guarded
BEFORE INSERT OR UPDATE OR DELETE ON tool_attempt
FOR EACH ROW
EXECUTE FUNCTION reject_tool_attempt_invalid_change();

-- Tool-use and result entries contain references only. Request content,
-- approval reasons, and execution output remain single-authority records.
ALTER TABLE semantic_transcript_entry
    ADD COLUMN tool_result_request_id uuid,
    ADD COLUMN tool_result_attempt_id uuid,
    DROP CONSTRAINT semantic_transcript_entry_payload_kind_closed,
    DROP CONSTRAINT semantic_transcript_entry_payload_shape,
    DROP CONSTRAINT semantic_transcript_entry_tool_use_unavailable;

ALTER TABLE semantic_transcript_entry
    ADD CONSTRAINT semantic_transcript_entry_payload_kind_closed
        CHECK (
            payload_kind IN (
                'origin_accepted_input',
                'steering_accepted_input',
                'turn_failed',
                'assistant_text',
                'assistant_tool_use',
                'tool_execution_result',
                'tool_denied',
                'tool_closed_by_turn_end',
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
                AND tool_result_request_id IS NULL
                AND tool_result_attempt_id IS NULL
                AND completed_turn_id IS NULL
                AND cancelled_turn_id IS NULL
            )
            OR (
                payload_kind = 'turn_failed'
                AND origin_accepted_input_id IS NULL
                AND steering_source_turn_id IS NULL
                AND failed_turn_id IS NOT NULL
                AND assistant_text_value IS NULL
                AND producing_model_call_id IS NULL
                AND assistant_tool_request_id IS NULL
                AND tool_result_request_id IS NULL
                AND tool_result_attempt_id IS NULL
                AND completed_turn_id IS NULL
                AND cancelled_turn_id IS NULL
            )
            OR (
                payload_kind = 'assistant_text'
                AND origin_accepted_input_id IS NULL
                AND steering_source_turn_id IS NULL
                AND failed_turn_id IS NULL
                AND assistant_text_value IS NOT NULL
                AND assistant_text_value <> ''
                AND producing_model_call_id IS NOT NULL
                AND assistant_tool_request_id IS NULL
                AND tool_result_request_id IS NULL
                AND tool_result_attempt_id IS NULL
                AND completed_turn_id IS NULL
                AND cancelled_turn_id IS NULL
            )
            OR (
                payload_kind = 'assistant_tool_use'
                AND origin_accepted_input_id IS NULL
                AND steering_source_turn_id IS NULL
                AND failed_turn_id IS NULL
                AND assistant_text_value IS NULL
                AND producing_model_call_id IS NOT NULL
                AND assistant_tool_request_id IS NOT NULL
                AND tool_result_request_id IS NULL
                AND tool_result_attempt_id IS NULL
                AND completed_turn_id IS NULL
                AND cancelled_turn_id IS NULL
            )
            OR (
                payload_kind = 'tool_execution_result'
                AND origin_accepted_input_id IS NULL
                AND steering_source_turn_id IS NULL
                AND failed_turn_id IS NULL
                AND assistant_text_value IS NULL
                AND producing_model_call_id IS NULL
                AND assistant_tool_request_id IS NULL
                AND tool_result_request_id IS NULL
                AND tool_result_attempt_id IS NOT NULL
                AND completed_turn_id IS NULL
                AND cancelled_turn_id IS NULL
            )
            OR (
                payload_kind IN ('tool_denied', 'tool_closed_by_turn_end')
                AND origin_accepted_input_id IS NULL
                AND steering_source_turn_id IS NULL
                AND failed_turn_id IS NULL
                AND assistant_text_value IS NULL
                AND producing_model_call_id IS NULL
                AND assistant_tool_request_id IS NULL
                AND tool_result_request_id IS NOT NULL
                AND tool_result_attempt_id IS NULL
                AND completed_turn_id IS NULL
                AND cancelled_turn_id IS NULL
            )
            OR (
                payload_kind = 'turn_completed'
                AND origin_accepted_input_id IS NULL
                AND steering_source_turn_id IS NULL
                AND failed_turn_id IS NULL
                AND assistant_text_value IS NULL
                AND producing_model_call_id IS NULL
                AND assistant_tool_request_id IS NULL
                AND tool_result_request_id IS NULL
                AND tool_result_attempt_id IS NULL
                AND completed_turn_id IS NOT NULL
                AND cancelled_turn_id IS NULL
            )
            OR (
                payload_kind = 'turn_cancelled'
                AND origin_accepted_input_id IS NULL
                AND steering_source_turn_id IS NULL
                AND failed_turn_id IS NULL
                AND assistant_text_value IS NULL
                AND producing_model_call_id IS NULL
                AND assistant_tool_request_id IS NULL
                AND tool_result_request_id IS NULL
                AND tool_result_attempt_id IS NULL
                AND completed_turn_id IS NULL
                AND cancelled_turn_id IS NOT NULL
            )
        ),
    ADD CONSTRAINT semantic_transcript_entry_tool_use_fk
        FOREIGN KEY (
            assistant_tool_request_id,
            producing_model_call_id,
            source_session_id
        )
        REFERENCES tool_request (
            request_id,
            producing_model_call_id,
            session_id
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    ADD CONSTRAINT semantic_transcript_entry_tool_result_request_fk
        FOREIGN KEY (tool_result_request_id)
        REFERENCES tool_request (request_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    ADD CONSTRAINT semantic_transcript_entry_tool_result_attempt_fk
        FOREIGN KEY (tool_result_attempt_id)
        REFERENCES tool_attempt (attempt_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    ADD CONSTRAINT semantic_transcript_entry_tool_result_request_once
        UNIQUE (tool_result_request_id),
    ADD CONSTRAINT semantic_transcript_entry_tool_result_attempt_once
        UNIQUE (tool_result_attempt_id);

-- A stored active phase names its exact tool batch and wait subject. Approval
-- waits have no live turn attempt. Serialized execution has one; ambiguity
-- retains the ended issuing attempt and exact terminal tool attempt.
ALTER TABLE turn_lifecycle
    ADD COLUMN active_tool_round_call_id uuid,
    ADD COLUMN approval_tool_request_id uuid,
    ADD COLUMN recovery_tool_attempt_id uuid,
    DROP CONSTRAINT turn_lifecycle_active_phase_closed,
    DROP CONSTRAINT turn_lifecycle_state_payload_shape;

ALTER TABLE turn_lifecycle
    ADD CONSTRAINT turn_lifecycle_active_phase_closed
        CHECK (
            active_phase_kind IS NULL
            OR active_phase_kind IN (
                'running',
                'awaiting_model_call_recovery',
                'awaiting_tool_approval',
                'awaiting_tool_recovery'
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
                AND active_tool_round_call_id IS NULL
                AND approval_tool_request_id IS NULL
                AND recovery_tool_attempt_id IS NULL
                AND terminal_attempt_id IS NULL
                AND terminal_model_call_id IS NULL
            )
            OR (
                state_kind = 'active'
                AND start_lineage_kind IS NOT NULL
                AND starting_frontier_id IS NOT NULL
                AND terminal_frontier_id IS NULL
                AND active_phase_kind = 'running'
                AND current_attempt_id IS NOT NULL
                AND terminal_disposition_kind IS NULL
                AND recovery_model_call_id IS NULL
                AND approval_tool_request_id IS NULL
                AND recovery_tool_attempt_id IS NULL
                AND terminal_attempt_id IS NULL
                AND terminal_model_call_id IS NULL
            )
            OR (
                state_kind = 'active'
                AND start_lineage_kind IS NOT NULL
                AND starting_frontier_id IS NOT NULL
                AND terminal_frontier_id IS NULL
                AND active_phase_kind = 'awaiting_model_call_recovery'
                AND current_attempt_id IS NOT NULL
                AND terminal_disposition_kind IS NULL
                AND recovery_model_call_id IS NOT NULL
                AND active_tool_round_call_id IS NULL
                AND approval_tool_request_id IS NULL
                AND recovery_tool_attempt_id IS NULL
                AND terminal_attempt_id IS NULL
                AND terminal_model_call_id IS NULL
            )
            OR (
                state_kind = 'active'
                AND start_lineage_kind IS NOT NULL
                AND starting_frontier_id IS NOT NULL
                AND terminal_frontier_id IS NULL
                AND active_phase_kind = 'awaiting_tool_approval'
                AND current_attempt_id IS NULL
                AND terminal_disposition_kind IS NULL
                AND recovery_model_call_id IS NULL
                AND active_tool_round_call_id IS NOT NULL
                AND approval_tool_request_id IS NOT NULL
                AND recovery_tool_attempt_id IS NULL
                AND terminal_attempt_id IS NULL
                AND terminal_model_call_id IS NULL
            )
            OR (
                state_kind = 'active'
                AND start_lineage_kind IS NOT NULL
                AND starting_frontier_id IS NOT NULL
                AND terminal_frontier_id IS NULL
                AND active_phase_kind = 'awaiting_tool_recovery'
                AND current_attempt_id IS NOT NULL
                AND terminal_disposition_kind IS NULL
                AND recovery_model_call_id IS NULL
                AND active_tool_round_call_id IS NOT NULL
                AND approval_tool_request_id IS NULL
                AND recovery_tool_attempt_id IS NOT NULL
                AND terminal_attempt_id IS NULL
                AND terminal_model_call_id IS NULL
            )
            OR (
                state_kind = 'terminal'
                AND start_lineage_kind IS NOT NULL
                AND starting_frontier_id IS NOT NULL
                AND terminal_frontier_id IS NOT NULL
                AND active_phase_kind IS NULL
                AND current_attempt_id IS NULL
                AND terminal_disposition_kind = 'failed'
                AND recovery_model_call_id IS NULL
                AND active_tool_round_call_id IS NULL
                AND approval_tool_request_id IS NULL
                AND recovery_tool_attempt_id IS NULL
                AND (
                    (
                        terminal_attempt_id IS NULL
                        AND terminal_model_call_id IS NULL
                    )
                    OR terminal_attempt_id IS NOT NULL
                )
            )
            OR (
                state_kind = 'terminal'
                AND start_lineage_kind IS NOT NULL
                AND starting_frontier_id IS NOT NULL
                AND terminal_frontier_id IS NOT NULL
                AND active_phase_kind IS NULL
                AND current_attempt_id IS NULL
                AND terminal_disposition_kind IN ('completed', 'refused')
                AND recovery_model_call_id IS NULL
                AND active_tool_round_call_id IS NULL
                AND approval_tool_request_id IS NULL
                AND recovery_tool_attempt_id IS NULL
                AND terminal_attempt_id IS NOT NULL
                AND terminal_model_call_id IS NOT NULL
            )
            OR (
                state_kind = 'terminal'
                AND start_lineage_kind IS NOT NULL
                AND starting_frontier_id IS NOT NULL
                AND terminal_frontier_id IS NOT NULL
                AND active_phase_kind IS NULL
                AND current_attempt_id IS NULL
                AND terminal_disposition_kind = 'cancelled'
                AND recovery_model_call_id IS NULL
                AND active_tool_round_call_id IS NULL
                AND approval_tool_request_id IS NULL
                AND recovery_tool_attempt_id IS NULL
                AND terminal_attempt_id IS NOT NULL
            )
            OR (
                state_kind = 'terminal'
                AND start_lineage_kind IS NOT NULL
                AND starting_frontier_id IS NOT NULL
                AND terminal_frontier_id IS NOT NULL
                AND active_phase_kind IS NULL
                AND current_attempt_id IS NULL
                AND terminal_disposition_kind = 'reconciliation_required'
                AND recovery_model_call_id IS NULL
                AND active_tool_round_call_id IS NULL
                AND approval_tool_request_id IS NULL
                AND recovery_tool_attempt_id IS NULL
                AND terminal_attempt_id IS NOT NULL
                AND terminal_model_call_id IS NOT NULL
            )
        ),
    ADD CONSTRAINT turn_lifecycle_active_tool_round_fk
        FOREIGN KEY (active_tool_round_call_id, turn_id, session_id)
        REFERENCES tool_round (
            producing_model_call_id,
            turn_id,
            session_id
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    ADD CONSTRAINT turn_lifecycle_approval_tool_request_fk
        FOREIGN KEY (approval_tool_request_id, turn_id, session_id)
        REFERENCES tool_request (request_id, turn_id, session_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    ADD CONSTRAINT turn_lifecycle_recovery_tool_attempt_fk
        FOREIGN KEY (recovery_tool_attempt_id, turn_id, session_id)
        REFERENCES tool_attempt (attempt_id, turn_id, session_id)
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
       AND OLD.active_phase_kind IN (
            'awaiting_model_call_recovery',
            'awaiting_tool_recovery'
       )
       AND NEW.state_kind = 'active'
    THEN
        RAISE EXCEPTION 'recovery wait cannot reopen without a recovery decision'
            USING ERRCODE = '23514';
    END IF;

    IF OLD.state_kind = 'active'
       AND OLD.active_phase_kind = 'running'
       AND NEW.state_kind = 'active'
       AND NEW.active_phase_kind = 'running'
       AND OLD.current_attempt_id IS DISTINCT FROM NEW.current_attempt_id
       AND (
            NEW.active_tool_round_call_id IS NULL
            OR NOT EXISTS (
                SELECT 1
                  FROM turn_attempt
                 WHERE turn_attempt_id = OLD.current_attempt_id
                   AND turn_id = OLD.turn_id
                   AND session_id = OLD.session_id
                   AND state_kind = 'ended'
                   AND end_disposition = 'yielded_to_durable_wait'
            )
       )
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

CREATE FUNCTION assert_tool_decision_command_final_state(
    checked_command_id uuid
)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    command_record decide_tool_request_command%ROWTYPE;
    approval_count bigint;
BEGIN
    SELECT *
      INTO command_record
      FROM decide_tool_request_command
     WHERE command_id = checked_command_id;
    IF NOT FOUND THEN
        RETURN;
    END IF;

    SELECT count(*)
      INTO approval_count
      FROM tool_approval_decision AS approval
     WHERE approval.owner_command_id = checked_command_id
       AND approval.request_id = command_record.request_id
       AND approval.decision_source = 'owner_command'
       AND approval.decision_kind = command_record.decision_kind
       AND approval.denial_reason
           IS NOT DISTINCT FROM command_record.denial_reason;

    IF (
        command_record.result_kind = 'applied'
        AND approval_count <> 1
    ) OR (
        command_record.result_kind = 'rejected'
        AND EXISTS (
            SELECT 1
              FROM tool_approval_decision
             WHERE owner_command_id = checked_command_id
        )
    ) THEN
        RAISE EXCEPTION
            'tool decision command lacks its exact approval effect'
            USING ERRCODE = '23514';
    END IF;
END;
$$;

CREATE FUNCTION require_tool_decision_command_final_state()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF TG_TABLE_NAME = 'decide_tool_request_command' THEN
        PERFORM assert_tool_decision_command_final_state(NEW.command_id);
    ELSE
        PERFORM assert_tool_decision_command_final_state(NEW.owner_command_id);
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER decide_tool_request_command_requires_effect
AFTER INSERT ON decide_tool_request_command
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_tool_decision_command_final_state();

CREATE CONSTRAINT TRIGGER owner_tool_approval_requires_command
AFTER INSERT ON tool_approval_decision
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
WHEN (NEW.owner_command_id IS NOT NULL)
EXECUTE FUNCTION require_tool_decision_command_final_state();

CREATE FUNCTION assert_tool_attempt_authorized(
    checked_attempt_id uuid
)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    checked_request uuid;
BEGIN
    SELECT request_id
      INTO checked_request
      FROM tool_attempt
     WHERE attempt_id = checked_attempt_id;
    IF NOT FOUND THEN
        RETURN;
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM tool_approval_decision
         WHERE request_id = checked_request
           AND decision_kind = 'approve'
    ) THEN
        RAISE EXCEPTION 'tool attempt lacks exact approval authority'
            USING
                ERRCODE = '23514',
                CONSTRAINT = 'tool_attempt_requires_approval';
    END IF;
END;
$$;

CREATE FUNCTION require_tool_attempt_authorized()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    PERFORM assert_tool_attempt_authorized(
        CASE WHEN TG_OP = 'DELETE' THEN OLD.attempt_id ELSE NEW.attempt_id END
    );
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER tool_attempt_requires_approval
AFTER INSERT OR UPDATE OR DELETE ON tool_attempt
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_tool_attempt_authorized();

CREATE FUNCTION require_denied_tool_without_attempt()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.decision_kind = 'deny'
       AND EXISTS (
            SELECT 1
              FROM tool_attempt
             WHERE request_id = NEW.request_id
       )
    THEN
        RAISE EXCEPTION 'denied tool request cannot have an attempt'
            USING
                ERRCODE = '23514',
                CONSTRAINT = 'denied_tool_request_has_no_attempt';
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER denied_tool_request_has_no_attempt
AFTER INSERT ON tool_approval_decision
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_denied_tool_without_attempt();

CREATE FUNCTION assert_tool_round_final_state(
    checked_model_call_id uuid
)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    round_record tool_round%ROWTYPE;
    source_frontier uuid;
    source_count numeric(20, 0);
    boundary_count numeric(20, 0);
    request_count bigint;
    assistant_part_count bigint;
    tool_use_count bigint;
    prefix_mismatch_count bigint;
    closed_result_count bigint;
BEGIN
    SELECT *
      INTO round_record
      FROM tool_round
     WHERE producing_model_call_id = checked_model_call_id;
    IF NOT FOUND THEN
        RETURN;
    END IF;

    SELECT call.context_frontier_id
      INTO source_frontier
      FROM model_call AS call
      JOIN turn_attempt AS attempt
        ON attempt.turn_attempt_id = call.turn_attempt_id
       AND attempt.turn_id = call.turn_id
       AND attempt.session_id = call.session_id
     WHERE call.model_call_id = checked_model_call_id
       AND call.turn_id = round_record.turn_id
       AND call.session_id = round_record.session_id
       AND call.state_kind = 'terminal'
       AND call.terminal_disposition_kind = 'completed'
       AND (
            (
                round_record.boundary_kind = 'continuing'
                AND attempt.state_kind = 'ended'
                AND attempt.end_disposition = 'yielded_to_durable_wait'
            )
            OR (
                round_record.boundary_kind = 'closed_by_turn_end'
                AND attempt.state_kind = 'ended'
                AND attempt.end_variant = 'after_cancellation'
                AND attempt.end_disposition = 'cancelled'
            )
       );
    IF NOT FOUND THEN
        RAISE EXCEPTION 'tool round lacks its completed producing call'
            USING ERRCODE = '23514';
    END IF;

    SELECT count(*)
      INTO request_count
      FROM tool_request
     WHERE producing_model_call_id = checked_model_call_id;
    IF request_count <> round_record.request_count
       OR EXISTS (
            SELECT 1
              FROM generate_series(
                    0,
                    round_record.request_count::bigint - 1
              ) AS expected(request_ordinal)
              LEFT JOIN tool_request AS request
                ON request.producing_model_call_id = checked_model_call_id
               AND request.request_ordinal = expected.request_ordinal
             WHERE request.request_id IS NULL
       )
    THEN
        RAISE EXCEPTION 'tool round request inventory is not gapless'
            USING ERRCODE = '23514';
    END IF;

    SELECT count(*)
      INTO assistant_part_count
      FROM semantic_transcript_entry
     WHERE source_session_id = round_record.session_id
       AND producing_model_call_id = checked_model_call_id
       AND payload_kind IN ('assistant_text', 'assistant_tool_use');
    SELECT count(*)
      INTO tool_use_count
      FROM semantic_transcript_entry
     WHERE source_session_id = round_record.session_id
       AND producing_model_call_id = checked_model_call_id
       AND payload_kind = 'assistant_tool_use';
    IF assistant_part_count <> round_record.response_part_count
       OR tool_use_count <> round_record.request_count
    THEN
        RAISE EXCEPTION 'tool round lacks its exact assistant entry inventory'
            USING ERRCODE = '23514';
    END IF;

    SELECT member_count
      INTO source_count
      FROM context_frontier
     WHERE owning_session_id = round_record.session_id
       AND context_frontier_id = source_frontier;
    SELECT member_count
      INTO boundary_count
      FROM context_frontier
     WHERE owning_session_id = round_record.session_id
       AND context_frontier_id = round_record.boundary_frontier_id;
    SELECT count(*)
      INTO prefix_mismatch_count
      FROM context_frontier_member AS source_member
      LEFT JOIN context_frontier_member AS boundary_member
        ON boundary_member.owning_session_id = source_member.owning_session_id
       AND boundary_member.context_frontier_id =
           round_record.boundary_frontier_id
       AND boundary_member.member_position = source_member.member_position
       AND boundary_member.source_session_id = source_member.source_session_id
       AND boundary_member.semantic_entry_id = source_member.semantic_entry_id
     WHERE source_member.owning_session_id = round_record.session_id
       AND source_member.context_frontier_id = source_frontier
       AND boundary_member.member_position IS NULL;

    IF prefix_mismatch_count <> 0
       OR boundary_count < source_count + round_record.response_part_count
       OR EXISTS (
            SELECT 1
              FROM semantic_transcript_entry AS entry
             WHERE entry.source_session_id = round_record.session_id
               AND entry.producing_model_call_id = checked_model_call_id
               AND entry.payload_kind IN (
                    'assistant_text',
                    'assistant_tool_use'
               )
               AND NOT EXISTS (
                    SELECT 1
                      FROM context_frontier_member AS member
                     WHERE member.owning_session_id = round_record.session_id
                       AND member.context_frontier_id =
                           round_record.boundary_frontier_id
                       AND member.source_session_id = entry.source_session_id
                       AND member.semantic_entry_id = entry.semantic_entry_id
               )
       )
    THEN
        RAISE EXCEPTION 'tool round frontier omits its ordered response'
            USING ERRCODE = '23514';
    END IF;

    IF round_record.boundary_kind = 'continuing' THEN
        IF boundary_count
               IS DISTINCT FROM source_count + round_record.response_part_count
        THEN
            RAISE EXCEPTION 'continuing tool round boundary has extra content'
                USING ERRCODE = '23514';
        END IF;
    ELSE
        SELECT count(*)
          INTO closed_result_count
          FROM semantic_transcript_entry AS entry
          JOIN tool_request AS request
            ON request.request_id = entry.tool_result_request_id
         WHERE request.producing_model_call_id = checked_model_call_id
           AND entry.payload_kind = 'tool_closed_by_turn_end';
        IF closed_result_count <> round_record.request_count
           OR NOT EXISTS (
                SELECT 1
                  FROM turn_lifecycle
                 WHERE turn_id = round_record.turn_id
                   AND session_id = round_record.session_id
                   AND state_kind = 'terminal'
                   AND terminal_disposition_kind = 'cancelled'
                   AND terminal_frontier_id =
                       round_record.boundary_frontier_id
           )
        THEN
            RAISE EXCEPTION 'closed tool round lacks exact turn-end resolution'
                USING ERRCODE = '23514';
        END IF;
    END IF;
END;
$$;

CREATE OR REPLACE FUNCTION assert_model_call_steering_final_state(
    checked_model_call_id uuid
)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    checked_session uuid;
    checked_turn uuid;
    predecessor_attempt uuid;
    checked_frontier uuid;
    starting_frontier uuid;
    starting_count numeric(20, 0);
    checked_count numeric(20, 0);
    result_boundary uuid;
    result_producing_call uuid;
    result_boundary_count numeric(20, 0);
    result_request_count bigint;
    suffix_start_count numeric(20, 0);
    suffix_count bigint;
    consumed_count bigint;
    malformed_result_count bigint;
    malformed_count bigint;
BEGIN
    SELECT
        call.session_id,
        call.turn_id,
        attempt.continued_from_attempt_id,
        call.context_frontier_id,
        lifecycle.starting_frontier_id
      INTO
        checked_session,
        checked_turn,
        predecessor_attempt,
        checked_frontier,
        starting_frontier
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
        RETURN;
    END IF;

    SELECT member_count
      INTO starting_count
      FROM context_frontier
     WHERE owning_session_id = checked_session
       AND context_frontier_id = starting_frontier;
    SELECT member_count
      INTO checked_count
      FROM context_frontier
     WHERE owning_session_id = checked_session
       AND context_frontier_id = checked_frontier;

    IF starting_count IS NULL
       OR checked_count IS NULL
       OR checked_count < starting_count
       OR (
            checked_frontier <> starting_frontier
            AND checked_count = starting_count
       )
       OR EXISTS (
            SELECT 1
              FROM context_frontier_member AS starting
              LEFT JOIN context_frontier_member AS checked
                ON checked.owning_session_id = starting.owning_session_id
               AND checked.context_frontier_id = checked_frontier
               AND checked.member_position = starting.member_position
             WHERE starting.owning_session_id = checked_session
               AND starting.context_frontier_id = starting_frontier
               AND ROW(
                    checked.source_session_id,
                    checked.semantic_entry_id
               ) IS DISTINCT FROM ROW(
                    starting.source_session_id,
                    starting.semantic_entry_id
               )
       )
    THEN
        RAISE EXCEPTION
            'model call frontier does not preserve its turn-start prefix'
            USING ERRCODE = '23514';
    END IF;

    suffix_start_count := starting_count;
    IF predecessor_attempt IS NOT NULL THEN
        SELECT
            round.boundary_frontier_id,
            producing_call.model_call_id,
            boundary.member_count,
            round.request_count
          INTO
            result_boundary,
            result_producing_call,
            result_boundary_count,
            result_request_count
          FROM model_call AS producing_call
          JOIN tool_round AS round
            ON round.producing_model_call_id = producing_call.model_call_id
           AND round.turn_id = producing_call.turn_id
           AND round.session_id = producing_call.session_id
          JOIN context_frontier AS boundary
            ON boundary.owning_session_id = round.session_id
           AND boundary.context_frontier_id = round.boundary_frontier_id
         WHERE producing_call.turn_attempt_id = predecessor_attempt
           AND producing_call.turn_id = checked_turn
           AND producing_call.session_id = checked_session
           AND producing_call.state_kind = 'terminal'
           AND producing_call.terminal_disposition_kind = 'completed'
           AND round.boundary_kind = 'continuing';

        IF NOT FOUND THEN
            RAISE EXCEPTION
                'continued model call lacks its predecessor tool round'
                USING ERRCODE = '23514';
        END IF;

        IF checked_count < result_boundary_count + result_request_count
           OR EXISTS (
                SELECT 1
                  FROM context_frontier_member AS boundary
                  LEFT JOIN context_frontier_member AS checked
                    ON checked.owning_session_id =
                       boundary.owning_session_id
                   AND checked.context_frontier_id = checked_frontier
                   AND checked.member_position = boundary.member_position
                 WHERE boundary.owning_session_id = checked_session
                   AND boundary.context_frontier_id = result_boundary
                   AND ROW(
                        checked.source_session_id,
                        checked.semantic_entry_id
                   ) IS DISTINCT FROM ROW(
                        boundary.source_session_id,
                        boundary.semantic_entry_id
                   )
           )
        THEN
            RAISE EXCEPTION
                'continued model call omits its tool-round boundary'
                USING ERRCODE = '23514';
        END IF;

        SELECT count(*)
          INTO malformed_result_count
          FROM generate_series(
                0,
                result_request_count - 1
          ) AS expected(request_ordinal)
          JOIN tool_request AS request
            ON request.producing_model_call_id = result_producing_call
           AND request.request_ordinal = expected.request_ordinal
          LEFT JOIN context_frontier_member AS member
            ON member.owning_session_id = checked_session
           AND member.context_frontier_id = checked_frontier
           AND member.member_position =
               result_boundary_count + expected.request_ordinal + 1
          LEFT JOIN semantic_transcript_entry AS entry
            ON entry.source_session_id = member.source_session_id
           AND entry.semantic_entry_id = member.semantic_entry_id
          LEFT JOIN tool_attempt AS attempt
            ON attempt.attempt_id = entry.tool_result_attempt_id
         WHERE member.source_session_id IS DISTINCT FROM checked_session
            OR (
                (
                    entry.payload_kind = 'tool_execution_result'
                    AND attempt.request_id = request.request_id
                )
                OR (
                    entry.payload_kind = 'tool_denied'
                    AND entry.tool_result_request_id = request.request_id
                )
            ) IS NOT TRUE;

        IF malformed_result_count <> 0 THEN
            RAISE EXCEPTION
                'continued model call lacks proposal-ordered tool results'
                USING ERRCODE = '23514';
        END IF;
        suffix_start_count :=
            result_boundary_count + result_request_count;
    END IF;

    SELECT count(*)
      INTO suffix_count
      FROM context_frontier_member
     WHERE owning_session_id = checked_session
       AND context_frontier_id = checked_frontier
       AND member_position > suffix_start_count;

    SELECT count(*)
      INTO consumed_count
      FROM accepted_input
     WHERE session_id = checked_session
       AND expected_active_turn_id = checked_turn
       AND disposition_kind = 'consumed_as_steering'
       AND consuming_model_call_id = checked_model_call_id;

    SELECT count(*)
      INTO malformed_count
      FROM context_frontier_member AS member
      LEFT JOIN semantic_transcript_entry AS entry
        ON entry.source_session_id = member.source_session_id
       AND entry.semantic_entry_id = member.semantic_entry_id
      LEFT JOIN accepted_input AS accepted
        ON accepted.accepted_input_id = entry.origin_accepted_input_id
       AND accepted.session_id = entry.source_session_id
     WHERE member.owning_session_id = checked_session
       AND member.context_frontier_id = checked_frontier
       AND member.member_position > suffix_start_count
       AND (
            entry.payload_kind IS DISTINCT FROM 'steering_accepted_input'
            OR entry.source_session_id IS DISTINCT FROM checked_session
            OR entry.steering_source_turn_id IS DISTINCT FROM checked_turn
            OR accepted.disposition_kind IS DISTINCT FROM
               'consumed_as_steering'
            OR accepted.expected_active_turn_id IS DISTINCT FROM checked_turn
            OR accepted.consuming_model_call_id IS DISTINCT FROM
               checked_model_call_id
       );

    IF suffix_count IS DISTINCT FROM consumed_count
       OR malformed_count <> 0
       OR EXISTS (
            SELECT 1
              FROM accepted_input AS earlier
              JOIN accepted_input AS consumed
                ON consumed.session_id = earlier.session_id
               AND consumed.expected_active_turn_id =
                   earlier.expected_active_turn_id
               AND consumed.disposition_kind = 'consumed_as_steering'
               AND consumed.consuming_model_call_id =
                   checked_model_call_id
               AND consumed.acceptance_position >
                   earlier.acceptance_position
             WHERE earlier.session_id = checked_session
               AND earlier.expected_active_turn_id = checked_turn
               AND earlier.disposition_kind IN (
                    'pending_steering',
                    'reclassified_as_turn_origin'
               )
       )
       OR EXISTS (
            SELECT 1
              FROM (
                    SELECT
                        accepted.acceptance_position,
                        row_number() OVER (
                            ORDER BY accepted.acceptance_position
                        ) AS acceptance_order,
                        row_number() OVER (
                            ORDER BY member.member_position
                        ) AS member_order
                      FROM context_frontier_member AS member
                      JOIN semantic_transcript_entry AS entry
                        ON entry.source_session_id =
                           member.source_session_id
                       AND entry.semantic_entry_id =
                           member.semantic_entry_id
                      JOIN accepted_input AS accepted
                        ON accepted.accepted_input_id =
                           entry.origin_accepted_input_id
                       AND accepted.session_id = entry.source_session_id
                     WHERE member.owning_session_id = checked_session
                       AND member.context_frontier_id = checked_frontier
                       AND member.member_position > suffix_start_count
              ) AS ordered
             WHERE ordered.acceptance_order <> ordered.member_order
       )
    THEN
        RAISE EXCEPTION
            'model call steering suffix is not the exact accepted order'
            USING ERRCODE = '23514';
    END IF;
END;
$$;

ALTER FUNCTION assert_model_call_final_state(uuid)
    RENAME TO assert_model_call_final_state_without_tool_round;

CREATE FUNCTION assert_model_call_final_state(
    checked_model_call_id uuid
)
RETURNS void
LANGUAGE plpgsql
AS $$
BEGIN
    IF EXISTS (
        SELECT 1
          FROM tool_round
         WHERE producing_model_call_id = checked_model_call_id
    ) THEN
        PERFORM assert_tool_round_final_state(checked_model_call_id);
    ELSE
        PERFORM assert_model_call_final_state_without_tool_round(
            checked_model_call_id
        );
    END IF;
END;
$$;

ALTER FUNCTION assert_turn_lifecycle_final_state(uuid)
    RENAME TO assert_turn_lifecycle_final_state_without_tool_loop;

CREATE OR REPLACE FUNCTION assert_turn_attempt_final_state(
    checked_turn_attempt_id uuid
)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    attempt_record turn_attempt%ROWTYPE;
BEGIN
    SELECT *
      INTO attempt_record
      FROM turn_attempt
     WHERE turn_attempt_id = checked_turn_attempt_id;
    IF NOT FOUND OR attempt_record.continued_from_attempt_id IS NULL THEN
        RETURN;
    END IF;

    IF NOT EXISTS (
        SELECT 1
          FROM turn_attempt AS predecessor
          JOIN model_call AS call
            ON call.turn_attempt_id = predecessor.turn_attempt_id
           AND call.turn_id = predecessor.turn_id
           AND call.session_id = predecessor.session_id
          JOIN tool_round AS round
            ON round.producing_model_call_id = call.model_call_id
           AND round.turn_id = call.turn_id
           AND round.session_id = call.session_id
         WHERE predecessor.turn_attempt_id =
               attempt_record.continued_from_attempt_id
           AND predecessor.turn_id = attempt_record.turn_id
           AND predecessor.session_id = attempt_record.session_id
           AND predecessor.state_kind = 'ended'
           AND predecessor.end_variant = 'without_stop'
           AND predecessor.end_disposition = 'yielded_to_durable_wait'
           AND call.state_kind = 'terminal'
           AND call.terminal_disposition_kind = 'completed'
           AND round.boundary_kind = 'continuing'
    ) THEN
        RAISE EXCEPTION
            'turn attempt continuation lacks an exact durable tool yield'
            USING
                ERRCODE = '23514',
                CONSTRAINT = 'turn_attempt_continuation_requires_tool_yield';
    END IF;
END;
$$;

CREATE FUNCTION assert_tool_loop_turn_final_state(
    checked_turn_id uuid
)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    lifecycle turn_lifecycle%ROWTYPE;
    attempt_count bigint;
    initial_attempt_count bigint;
    linked_attempt_count bigint;
    live_attempt_count bigint;
    unresolved_result_count bigint;
    completion_count bigint;
    failure_count bigint;
    cancellation_count bigint;
    round_id uuid;
BEGIN
    SELECT *
      INTO lifecycle
      FROM turn_lifecycle
     WHERE turn_id = checked_turn_id;
    IF NOT FOUND THEN
        RETURN;
    END IF;

    FOR round_id IN
        SELECT producing_model_call_id
          FROM tool_round
         WHERE turn_id = lifecycle.turn_id
           AND session_id = lifecycle.session_id
    LOOP
        PERFORM assert_tool_round_final_state(round_id);
    END LOOP;

    SELECT
        count(*),
        count(*) FILTER (WHERE continued_from_attempt_id IS NULL),
        count(*) FILTER (WHERE continued_from_attempt_id IS NOT NULL),
        count(*) FILTER (WHERE state_kind <> 'ended')
      INTO
        attempt_count,
        initial_attempt_count,
        linked_attempt_count,
        live_attempt_count
      FROM turn_attempt
     WHERE turn_id = lifecycle.turn_id
       AND session_id = lifecycle.session_id;

    IF lifecycle.attempt_history_present IS DISTINCT FROM (attempt_count > 0)
       OR initial_attempt_count <> 1
       OR linked_attempt_count <> attempt_count - 1
    THEN
        RAISE EXCEPTION 'tool-loop turn lacks one linear attempt history'
            USING ERRCODE = '23514';
    END IF;

    IF lifecycle.state_kind = 'active' THEN
        IF EXISTS (
            SELECT 1
              FROM semantic_transcript_entry
             WHERE source_session_id = lifecycle.session_id
               AND (
                    failed_turn_id = lifecycle.turn_id
                    OR completed_turn_id = lifecycle.turn_id
                    OR cancelled_turn_id = lifecycle.turn_id
               )
               AND payload_kind IN (
                    'turn_failed',
                    'turn_completed',
                    'turn_cancelled'
               )
        ) THEN
            RAISE EXCEPTION 'active tool-loop turn carries a terminal marker'
                USING ERRCODE = '23514';
        END IF;

        CASE lifecycle.active_phase_kind
            WHEN 'running' THEN
                IF live_attempt_count <> 1
                   OR NOT EXISTS (
                        SELECT 1
                          FROM turn_attempt
                         WHERE turn_attempt_id = lifecycle.current_attempt_id
                           AND turn_id = lifecycle.turn_id
                           AND session_id = lifecycle.session_id
                           AND state_kind <> 'ended'
                   )
                THEN
                    RAISE EXCEPTION
                        'running tool-loop turn lacks its exact live attempt'
                        USING ERRCODE = '23514';
                END IF;

                IF lifecycle.active_tool_round_call_id IS NOT NULL
                   AND (
                        EXISTS (
                            SELECT 1
                              FROM tool_request AS request
                              LEFT JOIN tool_approval_decision AS approval
                                ON approval.request_id = request.request_id
                             WHERE request.producing_model_call_id =
                                   lifecycle.active_tool_round_call_id
                               AND approval.request_id IS NULL
                        )
                        OR EXISTS (
                            SELECT 1
                              FROM tool_attempt AS attempt
                              JOIN tool_request AS request
                                ON request.request_id = attempt.request_id
                             WHERE request.producing_model_call_id =
                                   lifecycle.active_tool_round_call_id
                               AND attempt.issuing_turn_attempt_id
                                   <> lifecycle.current_attempt_id
                        )
                   )
                THEN
                    RAISE EXCEPTION
                        'executing tool batch lacks resolved serial authority'
                        USING ERRCODE = '23514';
                END IF;
            WHEN 'awaiting_tool_approval' THEN
                IF live_attempt_count <> 0
                   OR EXISTS (
                        SELECT 1
                          FROM tool_attempt AS attempt
                          JOIN tool_request AS request
                            ON request.request_id = attempt.request_id
                         WHERE request.producing_model_call_id =
                               lifecycle.active_tool_round_call_id
                   )
                   OR NOT EXISTS (
                        SELECT 1
                          FROM tool_request AS waiting
                          LEFT JOIN tool_approval_decision AS approval
                            ON approval.request_id = waiting.request_id
                         WHERE waiting.request_id =
                               lifecycle.approval_tool_request_id
                           AND waiting.producing_model_call_id =
                               lifecycle.active_tool_round_call_id
                           AND approval.request_id IS NULL
                           AND NOT EXISTS (
                                SELECT 1
                                  FROM tool_request AS earlier
                                  LEFT JOIN tool_approval_decision AS earlier_approval
                                    ON earlier_approval.request_id =
                                       earlier.request_id
                                 WHERE earlier.producing_model_call_id =
                                       waiting.producing_model_call_id
                                   AND earlier.request_ordinal <
                                       waiting.request_ordinal
                                   AND earlier_approval.request_id IS NULL
                           )
                   )
                THEN
                    RAISE EXCEPTION
                        'approval wait is not the earliest undecided request'
                        USING
                            ERRCODE = '23514',
                            CONSTRAINT =
                                'tool_approval_wait_earliest_undecided';
                END IF;
            WHEN 'awaiting_tool_recovery' THEN
                IF live_attempt_count <> 0
                   OR NOT EXISTS (
                        SELECT 1
                          FROM tool_attempt AS attempt
                          JOIN tool_request AS request
                            ON request.request_id = attempt.request_id
                         WHERE attempt.attempt_id =
                               lifecycle.recovery_tool_attempt_id
                           AND request.producing_model_call_id =
                               lifecycle.active_tool_round_call_id
                           AND attempt.issuing_turn_attempt_id =
                               lifecycle.current_attempt_id
                           AND attempt.state_kind = 'terminal'
                           AND attempt.terminal_disposition_kind = 'ambiguous'
                   )
                   OR NOT EXISTS (
                        SELECT 1
                          FROM turn_attempt
                         WHERE turn_attempt_id = lifecycle.current_attempt_id
                           AND turn_id = lifecycle.turn_id
                           AND session_id = lifecycle.session_id
                           AND state_kind = 'ended'
                           AND end_disposition IN ('ambiguous', 'lost')
                   )
                THEN
                    RAISE EXCEPTION 'tool recovery wait lacks exact ambiguity'
                        USING ERRCODE = '23514';
                END IF;
            WHEN 'awaiting_model_call_recovery' THEN
                IF live_attempt_count <> 0
                   OR NOT EXISTS (
                        SELECT 1
                          FROM model_call
                         WHERE model_call_id =
                               lifecycle.recovery_model_call_id
                           AND turn_attempt_id =
                               lifecycle.current_attempt_id
                           AND turn_id = lifecycle.turn_id
                           AND session_id = lifecycle.session_id
                           AND state_kind = 'terminal'
                           AND terminal_disposition_kind = 'ambiguous'
                   )
                THEN
                    RAISE EXCEPTION
                        'model recovery wait lacks exact ambiguity'
                        USING ERRCODE = '23514';
                END IF;
            ELSE
                RAISE EXCEPTION 'unsupported active tool-loop phase'
                    USING ERRCODE = '23514';
        END CASE;
        RETURN;
    END IF;

    IF lifecycle.state_kind <> 'terminal' OR live_attempt_count <> 0 THEN
        RAISE EXCEPTION 'tool-loop turn is neither active nor terminal'
            USING ERRCODE = '23514';
    END IF;

    SELECT count(*)
      INTO completion_count
      FROM semantic_transcript_entry
     WHERE source_session_id = lifecycle.session_id
       AND payload_kind = 'turn_completed'
       AND completed_turn_id = lifecycle.turn_id;
    SELECT count(*)
      INTO failure_count
      FROM semantic_transcript_entry
     WHERE source_session_id = lifecycle.session_id
       AND payload_kind = 'turn_failed'
       AND failed_turn_id = lifecycle.turn_id;
    SELECT count(*)
      INTO cancellation_count
      FROM semantic_transcript_entry
     WHERE source_session_id = lifecycle.session_id
       AND payload_kind = 'turn_cancelled'
       AND cancelled_turn_id = lifecycle.turn_id;

    IF (
        lifecycle.terminal_disposition_kind = 'completed'
        AND (
            completion_count <> 1
            OR failure_count <> 0
            OR cancellation_count <> 0
        )
    ) OR (
        lifecycle.terminal_disposition_kind = 'failed'
        AND (
            failure_count <> 1
            OR completion_count <> 0
            OR cancellation_count <> 0
        )
    ) OR (
        lifecycle.terminal_disposition_kind = 'cancelled'
        AND (
            cancellation_count <> 1
            OR completion_count <> 0
            OR failure_count <> 0
        )
    ) THEN
        RAISE EXCEPTION 'tool-loop terminal marker contradicts disposition'
            USING ERRCODE = '23514';
    END IF;

    IF lifecycle.terminal_disposition_kind IN ('completed', 'cancelled') THEN
        SELECT count(*)
          INTO unresolved_result_count
          FROM tool_request AS request
         WHERE request.turn_id = lifecycle.turn_id
           AND request.session_id = lifecycle.session_id
           AND NOT EXISTS (
                SELECT 1
                  FROM semantic_transcript_entry AS entry
                  LEFT JOIN tool_attempt AS attempt
                    ON attempt.attempt_id = entry.tool_result_attempt_id
                 WHERE entry.source_session_id = lifecycle.session_id
                   AND (
                        entry.tool_result_request_id = request.request_id
                        OR attempt.request_id = request.request_id
                   )
                   AND entry.payload_kind IN (
                        'tool_execution_result',
                        'tool_denied',
                        'tool_closed_by_turn_end'
                   )
           );
        IF unresolved_result_count <> 0 THEN
            RAISE EXCEPTION 'terminal tool-loop turn has unresolved requests'
                USING ERRCODE = '23514';
        END IF;
    END IF;
END;
$$;

CREATE FUNCTION assert_turn_lifecycle_final_state(
    checked_turn_id uuid
)
RETURNS void
LANGUAGE plpgsql
AS $$
BEGIN
    IF EXISTS (
        SELECT 1
          FROM tool_round
         WHERE turn_id = checked_turn_id
    ) THEN
        PERFORM assert_tool_loop_turn_final_state(checked_turn_id);
    ELSE
        PERFORM assert_turn_lifecycle_final_state_without_tool_loop(
            checked_turn_id
        );
    END IF;
END;
$$;

CREATE OR REPLACE FUNCTION require_semantic_entry_turn_state()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    entry semantic_transcript_entry%ROWTYPE;
    checked_turn_id uuid;
    checked_producing_call_id uuid;
BEGIN
    IF TG_OP = 'DELETE' THEN
        entry := OLD;
    ELSE
        entry := NEW;
    END IF;
    checked_producing_call_id := entry.producing_model_call_id;

    CASE entry.payload_kind
        WHEN 'origin_accepted_input' THEN
            SELECT origin_turn_id
              INTO checked_turn_id
              FROM accepted_input
             WHERE accepted_input_id = entry.origin_accepted_input_id
               AND session_id = entry.source_session_id
               AND disposition_kind IN (
                    'origin_of',
                    'reclassified_as_turn_origin'
               )
               AND origin_turn_id IS NOT NULL;
            IF NOT FOUND THEN
                RAISE EXCEPTION 'semantic origin input is not a turn origin'
                    USING
                        ERRCODE = '23514',
                        CONSTRAINT =
                            'semantic_transcript_entry_origin_disposition';
            END IF;
        WHEN 'steering_accepted_input' THEN
            SELECT expected_active_turn_id, consuming_model_call_id
              INTO checked_turn_id, checked_producing_call_id
              FROM accepted_input
             WHERE accepted_input_id = entry.origin_accepted_input_id
               AND session_id = entry.source_session_id
               AND disposition_kind = 'consumed_as_steering'
               AND expected_active_turn_id =
                   entry.steering_source_turn_id
               AND consuming_model_call_id IS NOT NULL;
            IF NOT FOUND THEN
                RAISE EXCEPTION
                    'semantic steering input lacks consuming call'
                    USING ERRCODE = '23514';
            END IF;
        WHEN 'turn_failed' THEN
            checked_turn_id := entry.failed_turn_id;
        WHEN 'turn_completed' THEN
            checked_turn_id := entry.completed_turn_id;
        WHEN 'turn_cancelled' THEN
            checked_turn_id := entry.cancelled_turn_id;
        WHEN 'assistant_text' THEN
            SELECT turn_id
              INTO checked_turn_id
              FROM model_call
             WHERE model_call_id = entry.producing_model_call_id
               AND state_kind = 'terminal'
               AND terminal_disposition_kind = 'completed';
        WHEN 'assistant_tool_use' THEN
            SELECT request.turn_id
              INTO checked_turn_id
              FROM tool_request AS request
             WHERE request.request_id = entry.assistant_tool_request_id
               AND request.producing_model_call_id =
                   entry.producing_model_call_id
               AND request.session_id = entry.source_session_id;
        WHEN 'tool_execution_result' THEN
            SELECT turn_id
              INTO checked_turn_id
              FROM tool_attempt
             WHERE attempt_id = entry.tool_result_attempt_id
               AND session_id = entry.source_session_id
               AND state_kind = 'terminal'
               AND (
                    terminal_disposition_kind = 'completed'
                    OR (
                        terminal_disposition_kind = 'known_failed'
                        AND error_kind <> 'crash_lost'
                    )
               );
        WHEN 'tool_denied' THEN
            SELECT request.turn_id
              INTO checked_turn_id
              FROM tool_request AS request
              JOIN tool_approval_decision AS approval
                ON approval.request_id = request.request_id
               AND approval.decision_kind = 'deny'
             WHERE request.request_id = entry.tool_result_request_id
               AND request.session_id = entry.source_session_id;
        WHEN 'tool_closed_by_turn_end' THEN
            SELECT request.turn_id
              INTO checked_turn_id
              FROM tool_request AS request
              JOIN turn_lifecycle AS lifecycle
                ON lifecycle.turn_id = request.turn_id
               AND lifecycle.session_id = request.session_id
               AND lifecycle.state_kind = 'terminal'
             WHERE request.request_id = entry.tool_result_request_id
               AND request.session_id = entry.source_session_id;
        ELSE
            RAISE EXCEPTION
                'semantic payload kind % lacks construction authority',
                entry.payload_kind
                USING ERRCODE = '23514';
    END CASE;

    IF checked_turn_id IS NULL THEN
        RAISE EXCEPTION 'semantic entry lacks authoritative turn'
            USING ERRCODE = '23514';
    END IF;
    PERFORM assert_turn_lifecycle_final_state(checked_turn_id);
    IF checked_producing_call_id IS NOT NULL THEN
        PERFORM assert_model_call_final_state(checked_producing_call_id);
    END IF;
    RETURN NULL;
END;
$$;

CREATE FUNCTION require_tool_loop_final_state()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    checked_turn uuid;
    checked_call uuid;
BEGIN
    IF TG_TABLE_NAME = 'tool_round' THEN
        checked_turn := NEW.turn_id;
        checked_call := NEW.producing_model_call_id;
    ELSIF TG_TABLE_NAME = 'tool_request' THEN
        checked_turn := NEW.turn_id;
        checked_call := NEW.producing_model_call_id;
    ELSIF TG_TABLE_NAME = 'tool_attempt' THEN
        checked_turn := CASE WHEN TG_OP = 'DELETE' THEN OLD.turn_id ELSE NEW.turn_id END;
    ELSIF TG_TABLE_NAME = 'tool_approval_decision' THEN
        SELECT turn_id, producing_model_call_id
          INTO checked_turn, checked_call
          FROM tool_request
         WHERE request_id = NEW.request_id;
    END IF;

    IF checked_call IS NOT NULL THEN
        PERFORM assert_tool_round_final_state(checked_call);
    END IF;
    IF checked_turn IS NOT NULL THEN
        PERFORM assert_turn_lifecycle_final_state(checked_turn);
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER tool_round_requires_complete_final_state
AFTER INSERT OR UPDATE OR DELETE ON tool_round
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_tool_loop_final_state();

CREATE CONSTRAINT TRIGGER tool_request_requires_complete_final_state
AFTER INSERT OR UPDATE OR DELETE ON tool_request
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_tool_loop_final_state();

CREATE CONSTRAINT TRIGGER tool_approval_requires_complete_final_state
AFTER INSERT OR UPDATE OR DELETE ON tool_approval_decision
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_tool_loop_final_state();

CREATE CONSTRAINT TRIGGER tool_attempt_requires_complete_final_state
AFTER INSERT OR UPDATE OR DELETE ON tool_attempt
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_tool_loop_final_state();
