-- Admit a newly established epoch as the recovery source when it supersedes
-- durable suspicion on the same runner enrollment.

CREATE OR REPLACE FUNCTION guard_runner_state_transition_outbox_event()
RETURNS trigger
LANGUAGE plpgsql
AS $function$
DECLARE
    placement runner_session_placement_record%ROWTYPE;
    prior runner_session_placement_record%ROWTYPE;
    connection runner_connection_event%ROWTYPE;
    predecessor runner_connection_event%ROWTYPE;
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
        PERFORM 1
          FROM runner_current_session_placement AS current_placement
         WHERE current_placement.session_id = placement.session_id
           AND current_placement.event_ordinal = placement.event_ordinal;
        IF NOT FOUND THEN
            RAISE EXCEPTION 'runner connection outbox source is not the current placement'
                USING ERRCODE = '23514';
        END IF;

        SELECT *
          INTO STRICT connection
          FROM runner_connection_event
         WHERE enrollment_id = NEW.connection_enrollment_id
           AND connection_epoch = NEW.connection_epoch
           AND event_ordinal = NEW.connection_event_ordinal;
        PERFORM 1
          FROM runner_connection_event AS later
         WHERE later.enrollment_id = connection.enrollment_id
           AND (
                later.connection_epoch > connection.connection_epoch
                OR (
                    later.connection_epoch = connection.connection_epoch
                    AND later.event_ordinal > connection.event_ordinal
                )
           );
        IF FOUND THEN
            RAISE EXCEPTION 'runner connection outbox source is not the latest event'
                USING ERRCODE = '23514';
        END IF;
        IF NEW.state_kind = 'connected'
           AND connection.cause_kind = 'established'
        THEN
            SELECT *
              INTO predecessor
              FROM runner_connection_event AS earlier
             WHERE earlier.enrollment_id = connection.enrollment_id
               AND (
                    earlier.connection_epoch < connection.connection_epoch
                    OR (
                        earlier.connection_epoch = connection.connection_epoch
                        AND earlier.event_ordinal < connection.event_ordinal
                    )
               )
             ORDER BY earlier.connection_epoch DESC, earlier.event_ordinal DESC
             LIMIT 1;
            IF NOT FOUND
               OR connection.event_ordinal <> 1
               OR predecessor.connection_epoch + 1 <>
                    connection.connection_epoch
               OR predecessor.state_kind <> 'suspect'
            THEN
                RAISE EXCEPTION 'established runner recovery lacks suspect predecessor'
                    USING ERRCODE = '23514';
            END IF;
        END IF;
        IF placement.state_kind <> 'pinned'
           OR placement.pinned_runner_id <> NEW.runner_id
           OR placement.registration_enrollment_id <>
                NEW.connection_enrollment_id
           OR (NEW.state_kind = 'suspect'
                AND ROW(connection.state_kind, connection.cause_kind) <>
                    ROW('suspect', 'heartbeat_missed'))
           OR (NEW.state_kind = 'connected'
                AND connection.state_kind <> 'connected')
           OR (NEW.state_kind = 'connected'
                AND connection.cause_kind NOT IN (
                    'established',
                    'heartbeat_recovered'
                ))
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

CREATE INDEX runner_session_placement_record_enrollment_pin_lookup
    ON runner_session_placement_record (
        registration_enrollment_id,
        session_id,
        event_ordinal
    )
    WHERE state_kind = 'pinned';
