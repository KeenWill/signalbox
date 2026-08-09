-- Typed durable runner-state transitions for session followers.

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
                'turn_reconciliation_required',
                'runner_state_transition'
            )
        );

CREATE TABLE runner_state_transition_outbox_event (
    event_sequence numeric(20, 0) PRIMARY KEY,
    event_kind text NOT NULL,
    storage_version smallint NOT NULL,
    session_id uuid NOT NULL,
    runner_id uuid NOT NULL,
    placement_revision numeric(20, 0) NOT NULL,
    sandbox_profile text NOT NULL,
    working_directory runner_exact_text,
    state_kind text NOT NULL,
    placement_event_ordinal numeric(20, 0) NOT NULL,
    connection_enrollment_id uuid,
    connection_epoch numeric(20, 0),
    connection_event_ordinal numeric(20, 0),

    CONSTRAINT runner_state_transition_outbox_kind_closed
        CHECK (event_kind = 'runner_state_transition'),
    CONSTRAINT runner_state_transition_outbox_version_supported
        CHECK (storage_version = 1),
    CONSTRAINT runner_state_transition_outbox_positive_u64
        CHECK (
            placement_revision BETWEEN 1 AND 18446744073709551615
            AND placement_event_ordinal BETWEEN 1 AND 18446744073709551615
            AND (
                connection_epoch IS NULL
                OR connection_epoch BETWEEN 1 AND 18446744073709551615
            )
            AND (
                connection_event_ordinal IS NULL
                OR connection_event_ordinal BETWEEN 1 AND 18446744073709551615
            )
        ),
    CONSTRAINT runner_state_transition_outbox_sandbox_closed
        CHECK (sandbox_profile IN ('ambient', 'workspace_restricted')),
    CONSTRAINT runner_state_transition_outbox_state_closed
        CHECK (
            state_kind IN (
                'pinned',
                'suspect',
                'connected',
                'runner_lost_before_pin',
                'runner_lost',
                'replaced',
                'working_directory_changed',
                'abandoned'
            )
        ),
    CONSTRAINT runner_state_transition_outbox_source_shape
        CHECK (
            (
                state_kind IN ('suspect', 'connected')
                AND connection_enrollment_id IS NOT NULL
                AND connection_epoch IS NOT NULL
                AND connection_event_ordinal IS NOT NULL
            )
            OR (
                state_kind NOT IN ('suspect', 'connected')
                AND connection_enrollment_id IS NULL
                AND connection_epoch IS NULL
                AND connection_event_ordinal IS NULL
            )
        ),
    CONSTRAINT runner_state_transition_outbox_source_key
        UNIQUE NULLS NOT DISTINCT (
            session_id,
            placement_event_ordinal,
            connection_enrollment_id,
            connection_epoch,
            connection_event_ordinal
        ),
    CONSTRAINT runner_state_transition_outbox_header_fk
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
    CONSTRAINT runner_state_transition_outbox_placement_fk
        FOREIGN KEY (
            session_id,
            placement_event_ordinal,
            placement_revision
        )
        REFERENCES runner_session_placement_record (
            session_id,
            event_ordinal,
            placement_revision
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CONSTRAINT runner_state_transition_outbox_connection_fk
        FOREIGN KEY (
            connection_enrollment_id,
            connection_epoch,
            connection_event_ordinal
        )
        REFERENCES runner_connection_event (
            enrollment_id,
            connection_epoch,
            event_ordinal
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
);

CREATE FUNCTION guard_runner_state_transition_outbox_event()
RETURNS trigger
LANGUAGE plpgsql
AS $function$
DECLARE
    placement runner_session_placement_record%ROWTYPE;
    prior runner_session_placement_record%ROWTYPE;
    connection runner_connection_event%ROWTYPE;
    expected_runner uuid;
BEGIN
    SELECT *
      INTO STRICT placement
      FROM runner_session_placement_record
     WHERE session_id = NEW.session_id
       AND event_ordinal = NEW.placement_event_ordinal
       AND placement_revision = NEW.placement_revision;

    IF NEW.sandbox_profile <> placement.requested_sandbox_profile
       OR NEW.working_directory IS DISTINCT FROM
            placement.requested_working_directory
    THEN
        RAISE EXCEPTION 'runner outbox placement projection does not match source'
            USING ERRCODE = '23514';
    END IF;

    IF NEW.state_kind IN ('suspect', 'connected') THEN
        SELECT *
          INTO STRICT connection
          FROM runner_connection_event
         WHERE enrollment_id = NEW.connection_enrollment_id
           AND connection_epoch = NEW.connection_epoch
           AND event_ordinal = NEW.connection_event_ordinal;
        IF placement.state_kind <> 'pinned'
           OR placement.pinned_runner_id <> NEW.runner_id
           OR placement.registration_enrollment_id <>
                NEW.connection_enrollment_id
           OR (NEW.state_kind = 'suspect'
                AND ROW(connection.state_kind, connection.cause_kind) <>
                    ROW('suspect', 'heartbeat_missed'))
           OR (NEW.state_kind = 'connected'
                AND ROW(connection.state_kind, connection.cause_kind) <>
                    ROW('connected', 'heartbeat_recovered'))
        THEN
            RAISE EXCEPTION 'runner connection outbox source does not match placement'
                USING ERRCODE = '23514';
        END IF;
        RETURN NEW;
    END IF;

    IF NEW.state_kind = 'pinned' THEN
        expected_runner := placement.pinned_runner_id;
        IF placement.event_kind <> 'pinned'
           OR placement.state_kind <> 'pinned'
        THEN
            RAISE EXCEPTION 'runner pinned outbox source is not a pin'
                USING ERRCODE = '23514';
        END IF;
    ELSIF NEW.state_kind = 'runner_lost_before_pin' THEN
        expected_runner := placement.lost_runner_id;
        IF placement.event_kind <> 'runner_lost_before_pin'
           OR placement.state_kind <> 'runner_lost_before_pin'
        THEN
            RAISE EXCEPTION 'pre-pin loss outbox source is not runner loss'
                USING ERRCODE = '23514';
        END IF;
    ELSIF NEW.state_kind = 'runner_lost' THEN
        expected_runner := placement.lost_runner_id;
        IF placement.event_kind <> 'runner_lost'
           OR placement.state_kind <> 'runner_lost'
        THEN
            RAISE EXCEPTION 'runner loss outbox source is not runner loss'
                USING ERRCODE = '23514';
        END IF;
    ELSIF NEW.state_kind = 'replaced' THEN
        IF placement.event_kind = 'pre_pin_replaced'
           AND placement.state_kind = 'unpinned'
        THEN
            expected_runner := placement.selector_runner_id;
        ELSIF placement.event_kind = 'runner_replaced'
              AND placement.state_kind = 'pinned'
        THEN
            SELECT *
              INTO prior
              FROM runner_session_placement_record
             WHERE session_id = placement.session_id
               AND event_ordinal = placement.event_ordinal - 1;
            IF NOT FOUND THEN
                RAISE EXCEPTION 'runner replacement outbox source lacks its predecessor'
                    USING ERRCODE = '23514';
            END IF;
            IF prior.lost_runner_id IS NOT DISTINCT FROM placement.pinned_runner_id
               AND prior.requested_working_directory IS DISTINCT FROM
                    placement.requested_working_directory
            THEN
                RAISE EXCEPTION 'same-runner directory relocation requires its exact outbox state'
                    USING ERRCODE = '23514';
            END IF;
            expected_runner := placement.pinned_runner_id;
        ELSE
            RAISE EXCEPTION 'runner replacement outbox source is not replacement'
                USING ERRCODE = '23514';
        END IF;
    ELSIF NEW.state_kind = 'working_directory_changed' THEN
        SELECT *
          INTO prior
          FROM runner_session_placement_record
         WHERE session_id = placement.session_id
           AND event_ordinal = placement.event_ordinal - 1;
        IF NOT FOUND THEN
            RAISE EXCEPTION 'working-directory outbox source lacks its predecessor'
                USING ERRCODE = '23514';
        END IF;
        expected_runner := placement.pinned_runner_id;
        IF placement.event_kind <> 'runner_replaced'
           OR placement.state_kind <> 'pinned'
           OR prior.lost_runner_id IS DISTINCT FROM placement.pinned_runner_id
           OR prior.requested_working_directory IS NOT DISTINCT FROM
                placement.requested_working_directory
        THEN
            RAISE EXCEPTION 'working-directory outbox source is not relocation'
                USING ERRCODE = '23514';
        END IF;
    ELSIF NEW.state_kind = 'abandoned' THEN
        expected_runner := placement.lost_runner_id;
        IF placement.event_kind <> 'abandoned'
           OR placement.state_kind <> 'runner_abandoned'
        THEN
            RAISE EXCEPTION 'runner abandonment outbox source is not abandonment'
                USING ERRCODE = '23514';
        END IF;
    ELSE
        RAISE EXCEPTION 'unsupported runner outbox state %', NEW.state_kind
            USING ERRCODE = '23514';
    END IF;

    IF expected_runner IS NULL OR expected_runner <> NEW.runner_id THEN
        RAISE EXCEPTION 'runner outbox identity does not match source'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$function$;

CREATE TRIGGER runner_state_transition_outbox_event_is_guarded
BEFORE INSERT ON runner_state_transition_outbox_event
FOR EACH ROW
EXECUTE FUNCTION guard_runner_state_transition_outbox_event();

CREATE TRIGGER runner_state_transition_outbox_event_is_append_only
BEFORE UPDATE OR DELETE ON runner_state_transition_outbox_event
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER runner_state_transition_outbox_event_cannot_be_truncated
BEFORE TRUNCATE ON runner_state_transition_outbox_event
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
        WHEN 'runner_state_transition' THEN
            SELECT count(*) INTO matching_records
              FROM runner_state_transition_outbox_event
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
