-- Bind every runner-selected placement record to the latest connection-loss
-- epoch it was authorized after.  A later loss therefore keeps fencing that
-- placement even when the enrollment opens a successor physical connection.

ALTER TABLE runner_session_placement_record
    ADD COLUMN loss_fence_enrollment_id uuid,
    ADD COLUMN observed_runner_loss_epoch numeric(20, 0),
    ADD CONSTRAINT runner_session_placement_observed_loss_positive CHECK (
        observed_runner_loss_epoch IS NULL
        OR observed_runner_loss_epoch BETWEEN 1 AND 18446744073709551615
    ),
    ADD CONSTRAINT runner_session_placement_loss_fence_shape CHECK (
        (loss_fence_enrollment_id IS NULL
            AND observed_runner_loss_epoch IS NULL)
        OR loss_fence_enrollment_id IS NOT NULL
    ),
    ADD CONSTRAINT runner_session_placement_loss_fence_enrollment_fk
        FOREIGN KEY (loss_fence_enrollment_id)
        REFERENCES runner_enrollment (enrollment_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    ADD CONSTRAINT runner_session_placement_observed_loss_fk
        FOREIGN KEY (
            loss_fence_enrollment_id,
            observed_runner_loss_epoch
        )
        REFERENCES runner_connection_loss_epoch (enrollment_id, loss_epoch)
        ON UPDATE RESTRICT ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED;

DO $migration$
BEGIN
    IF EXISTS (SELECT 1 FROM runner_session_placement_record) THEN
        RAISE EXCEPTION
            'runner placement loss fence requires empty placement history';
    END IF;
END;
$migration$;

CREATE FUNCTION set_runner_placement_loss_baseline()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    selected_runner uuid;
    selected_enrollment uuid;
    current_loss_epoch numeric;
    prior runner_session_placement_record%ROWTYPE;
BEGIN
    IF NEW.loss_fence_enrollment_id IS NOT NULL
       OR NEW.observed_runner_loss_epoch IS NOT NULL
    THEN
        RAISE EXCEPTION 'runner placement loss baseline is adapter-derived'
            USING ERRCODE = '23514';
    END IF;

    -- Placement mutation shares the runner total order with loss propagation:
    -- scheduler, enrollment, connection/loss, then the placement insert.
    PERFORM 1
      FROM session_scheduler
     WHERE session_id = NEW.session_id
     FOR UPDATE;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'runner placement lacks its session scheduler'
            USING ERRCODE = '23514';
    END IF;

    IF NEW.event_ordinal > 1 THEN
        SELECT * INTO prior
          FROM runner_session_placement_record
         WHERE session_id = NEW.session_id
           AND event_ordinal = NEW.event_ordinal - 1;
        IF NOT FOUND THEN
            RAISE EXCEPTION 'runner placement loss baseline lacks its predecessor'
                USING ERRCODE = '23514';
        END IF;
    END IF;

    selected_runner := COALESCE(
        NEW.pinned_runner_id,
        NEW.selector_runner_id,
        NEW.lost_runner_id
    );
    IF selected_runner IS NULL THEN
        RETURN NEW;
    END IF;

    SELECT enrollment_id
      INTO selected_enrollment
      FROM runner_enrollment
     WHERE runner_id = selected_runner
     FOR SHARE;
    IF NOT FOUND THEN
        RETURN NEW;
    END IF;

    PERFORM 1
      FROM runner_connection_authority_head
     WHERE enrollment_id = selected_enrollment
     FOR SHARE;
    SELECT loss_epoch
      INTO current_loss_epoch
      FROM runner_current_connection_loss
     WHERE enrollment_id = selected_enrollment
     FOR SHARE;

    IF NEW.event_kind IN (
        'runner_lost_before_pin', 'runner_lost', 'abandoned'
    ) THEN
        IF prior.loss_fence_enrollment_id IS NOT NULL
           AND prior.loss_fence_enrollment_id IS DISTINCT FROM
                selected_enrollment
        THEN
            RAISE EXCEPTION
                'runner placement loss changed its enrollment baseline'
                USING ERRCODE = '23514';
        END IF;
        NEW.loss_fence_enrollment_id := prior.loss_fence_enrollment_id;
        NEW.observed_runner_loss_epoch := prior.observed_runner_loss_epoch;
    ELSIF NEW.event_kind = 'pinned'
          AND prior.selector_kind = 'identity'
          AND prior.loss_fence_enrollment_id IS NULL
    THEN
        IF current_loss_epoch IS NOT NULL THEN
            RAISE EXCEPTION
                'runner placement predecessor is fenced by connection loss'
                USING ERRCODE = '23514';
        END IF;
        NEW.loss_fence_enrollment_id := selected_enrollment;
        NEW.observed_runner_loss_epoch := NULL;
    ELSIF NEW.event_kind = 'profile_replaced' OR (
        NEW.event_kind = 'pinned'
        AND prior.selector_kind = 'identity'
    ) THEN
        IF prior.loss_fence_enrollment_id IS DISTINCT FROM selected_enrollment
           OR prior.observed_runner_loss_epoch IS DISTINCT FROM
                current_loss_epoch
        THEN
            RAISE EXCEPTION
                'runner placement predecessor is fenced by connection loss'
                USING ERRCODE = '23514';
        END IF;
        NEW.loss_fence_enrollment_id := prior.loss_fence_enrollment_id;
        NEW.observed_runner_loss_epoch := prior.observed_runner_loss_epoch;
    ELSE
        NEW.loss_fence_enrollment_id := selected_enrollment;
        NEW.observed_runner_loss_epoch := current_loss_epoch;
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER runner_session_placement_00_sets_loss_baseline
BEFORE INSERT ON runner_session_placement_record
FOR EACH ROW
EXECUTE FUNCTION set_runner_placement_loss_baseline();

CREATE OR REPLACE FUNCTION reject_runner_lease_generation_after_connection_loss()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    authority runner_connection_authority_head%ROWTYPE;
    connection runner_connection_event%ROWTYPE;
    loss runner_connection_loss_epoch%ROWTYPE;
    placement runner_session_placement_record%ROWTYPE;
BEGIN
    IF NEW.offer_connection_epoch IS NOT NULL
       OR NEW.offer_connection_event_ordinal IS NOT NULL
       OR NEW.offer_loss_epoch IS NOT NULL
    THEN
        RAISE EXCEPTION 'runner lease offer authority is adapter-owned'
            USING ERRCODE = '23514';
    END IF;
    PERFORM 1
      FROM runner_enrollment
     WHERE enrollment_id = NEW.registration_enrollment_id
     FOR SHARE;
    SELECT *
      INTO authority
      FROM runner_connection_authority_head
     WHERE enrollment_id = NEW.registration_enrollment_id
     FOR SHARE;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'runner lease offer lacks connection authority'
            USING ERRCODE = '23514';
    END IF;
    SELECT *
      INTO placement
      FROM runner_session_placement_record
     WHERE session_id = NEW.session_id
       AND event_ordinal = NEW.placement_event_ordinal;
    IF NOT FOUND
       OR placement.loss_fence_enrollment_id IS DISTINCT FROM
            NEW.registration_enrollment_id
    THEN
        RAISE EXCEPTION 'runner lease lacks its placement loss baseline'
            USING ERRCODE = '23514';
    END IF;
    IF authority.latest_loss_epoch IS NOT NULL
       AND (
            placement.observed_runner_loss_epoch IS NULL
            OR placement.observed_runner_loss_epoch <
                authority.latest_loss_epoch
       )
    THEN
        RAISE EXCEPTION 'runner placement is fenced by connection loss'
            USING ERRCODE = '23514';
    END IF;
    SELECT *
      INTO connection
      FROM runner_connection_event
     WHERE enrollment_id = authority.enrollment_id
       AND connection_epoch = authority.connection_epoch
       AND event_ordinal = authority.connection_event_ordinal;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'runner connection authority head lacks its event'
            USING ERRCODE = '23514';
    END IF;
    IF connection.state_kind = 'shutdown' THEN
        RAISE EXCEPTION 'shutdown runner connection cannot authorize a lease offer'
            USING ERRCODE = '23514';
    END IF;
    IF connection.state_kind IS DISTINCT FROM 'lost' THEN
        NEW.offer_connection_epoch := authority.connection_epoch;
        NEW.offer_connection_event_ordinal :=
            authority.connection_event_ordinal;
        NEW.offer_loss_epoch := authority.latest_loss_epoch;
        RETURN NEW;
    END IF;
    SELECT epoch.*
      INTO loss
      FROM runner_current_connection_loss AS current_loss
      JOIN runner_connection_loss_epoch AS epoch
        ON epoch.enrollment_id = current_loss.enrollment_id
       AND epoch.loss_epoch = current_loss.loss_epoch
     WHERE current_loss.enrollment_id = authority.enrollment_id
     FOR SHARE OF current_loss;
    IF authority.latest_loss_epoch IS DISTINCT FROM loss.loss_epoch
       OR loss.connection_epoch IS DISTINCT FROM connection.connection_epoch
       OR loss.connection_event_ordinal IS DISTINCT FROM
            connection.event_ordinal
    THEN
        RAISE EXCEPTION 'lost runner connection lacks its current epoch fence'
            USING ERRCODE = '23514';
    END IF;
    RAISE EXCEPTION 'lost runner connection cannot authorize a lease offer'
        USING ERRCODE = '23514';
END;
$$;
