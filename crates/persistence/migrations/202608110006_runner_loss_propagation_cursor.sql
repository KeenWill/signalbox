-- Every durable runner loss owns one restartable, bounded session-propagation
-- cursor. Losses already absorbed by migration 202608110005's compatibility
-- baseline begin complete without inventing a processed session identity. A
-- loss committed after that migration but before this one begins pending when
-- an affected current placement still retains an older baseline.

-- An exact-identity placement may begin before its runner enrolls. The runner
-- identity advisory lock serializes that absence observation with enrollment
-- insertion and with cursor completion, so an uncommitted null-baseline
-- placement cannot appear behind a completed loss cursor.

CREATE FUNCTION lock_runner_loss_identity(checked_runner uuid)
RETURNS void
LANGUAGE plpgsql
AS $$
BEGIN
    PERFORM pg_advisory_xact_lock(
        hashtextextended(
            'signalbox.runner-loss-identity.' || checked_runner::text,
            0
        )
    );
END;
$$;

CREATE FUNCTION serialize_runner_enrollment_loss_identity()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    PERFORM lock_runner_loss_identity(NEW.runner_id);
    RETURN NEW;
END;
$$;

CREATE TRIGGER runner_enrollment_00_serializes_loss_identity
BEFORE INSERT ON runner_enrollment
FOR EACH ROW
EXECUTE FUNCTION serialize_runner_enrollment_loss_identity();

CREATE FUNCTION serialize_runner_placement_loss_identity()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    selected_runner uuid;
BEGIN
    -- This trigger runs immediately before the baseline trigger. It extends
    -- that trigger's existing total order with the absence fence without
    -- duplicating its baseline predicate.
    PERFORM 1
      FROM session_scheduler
     WHERE session_id = NEW.session_id
     FOR UPDATE;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'runner placement lacks its session scheduler'
            USING ERRCODE = '23514';
    END IF;

    selected_runner := COALESCE(
        NEW.pinned_runner_id,
        NEW.selector_runner_id,
        NEW.lost_runner_id
    );
    IF selected_runner IS NULL THEN
        RETURN NEW;
    END IF;

    PERFORM lock_runner_loss_identity(selected_runner);
    RETURN NEW;
END;
$$;

ALTER TRIGGER runner_session_placement_00_sets_loss_baseline
ON runner_session_placement_record
RENAME TO runner_session_placement_01_sets_loss_baseline;

CREATE TRIGGER runner_session_placement_00_serializes_loss_identity
BEFORE INSERT ON runner_session_placement_record
FOR EACH ROW
EXECUTE FUNCTION serialize_runner_placement_loss_identity();

CREATE TABLE runner_connection_loss_propagation (
    enrollment_id uuid NOT NULL,
    loss_epoch numeric(20, 0) NOT NULL,
    propagated_through_session_id uuid,
    state_kind text NOT NULL,

    CONSTRAINT runner_connection_loss_propagation_pk
        PRIMARY KEY (enrollment_id, loss_epoch),
    CONSTRAINT runner_connection_loss_propagation_state_closed
        CHECK (state_kind IN ('pending', 'completed')),
    CONSTRAINT runner_connection_loss_propagation_loss_fk
        FOREIGN KEY (enrollment_id, loss_epoch)
        REFERENCES runner_connection_loss_epoch (enrollment_id, loss_epoch)
        ON UPDATE RESTRICT ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT runner_connection_loss_propagation_session_fk
        FOREIGN KEY (propagated_through_session_id)
        REFERENCES session (session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE INDEX runner_session_placement_loss_propagation_page
    ON runner_session_placement_record (
        loss_fence_enrollment_id,
        session_id,
        event_ordinal
    );

CREATE INDEX runner_session_placement_exact_loss_propagation_page
    ON runner_session_placement_record (
        selector_runner_id,
        session_id,
        event_ordinal
    )
    WHERE loss_fence_enrollment_id IS NULL
      AND state_kind = 'unpinned'
      AND selector_kind = 'identity';

INSERT INTO runner_connection_loss_propagation (
    enrollment_id,
    loss_epoch,
    propagated_through_session_id,
    state_kind
)
SELECT loss.enrollment_id,
       loss.loss_epoch,
       NULL,
       CASE
           WHEN EXISTS (
               SELECT 1
                 FROM runner_current_session_placement AS current_placement
                 JOIN runner_session_placement_record AS placement
                   ON placement.session_id = current_placement.session_id
                  AND placement.event_ordinal = current_placement.event_ordinal
                 JOIN runner_enrollment AS lost_enrollment
                   ON lost_enrollment.enrollment_id = loss.enrollment_id
                WHERE (
                       placement.loss_fence_enrollment_id = loss.enrollment_id
                       OR (
                           placement.loss_fence_enrollment_id IS NULL
                           AND placement.state_kind = 'unpinned'
                           AND placement.selector_kind = 'identity'
                           AND placement.selector_runner_id =
                               lost_enrollment.runner_id
                       )
                  )
                  AND (
                       placement.observed_runner_loss_epoch IS NULL
                       OR placement.observed_runner_loss_epoch < loss.loss_epoch
                  )
                  AND (
                       placement.state_kind = 'pinned'
                       OR (
                           placement.state_kind = 'unpinned'
                           AND placement.selector_kind = 'identity'
                       )
                  )
           ) THEN 'pending'
           ELSE 'completed'
       END
  FROM runner_connection_loss_epoch AS loss;

CREATE FUNCTION guard_runner_connection_loss_propagation()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    lost_runner uuid;
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'runner connection loss propagation is durable'
            USING ERRCODE = '23514';
    END IF;
    IF TG_OP = 'INSERT' THEN
        IF NEW.state_kind <> 'pending'
           OR NEW.propagated_through_session_id IS NOT NULL
        THEN
            RAISE EXCEPTION 'new runner loss propagation must start pending'
                USING ERRCODE = '23514';
        END IF;
        RETURN NEW;
    END IF;
    IF NEW.enrollment_id IS DISTINCT FROM OLD.enrollment_id
       OR NEW.loss_epoch IS DISTINCT FROM OLD.loss_epoch
       OR OLD.state_kind = 'completed'
       OR (
            NEW.state_kind = 'pending'
            AND (
                NEW.propagated_through_session_id IS NULL
                OR (
                    OLD.propagated_through_session_id IS NOT NULL
                    AND NEW.propagated_through_session_id <=
                        OLD.propagated_through_session_id
                )
            )
       )
       OR (
            NEW.state_kind = 'completed'
            AND NEW.propagated_through_session_id IS DISTINCT FROM
                OLD.propagated_through_session_id
       )
    THEN
        RAISE EXCEPTION 'runner connection loss propagation must advance monotonically'
            USING ERRCODE = '23514';
    END IF;
    IF NEW.state_kind = 'completed' THEN
        SELECT runner_id
          INTO STRICT lost_runner
          FROM runner_enrollment
         WHERE enrollment_id = NEW.enrollment_id;
        PERFORM lock_runner_loss_identity(lost_runner);
    END IF;
    IF NEW.state_kind = 'pending'
       AND EXISTS (
            SELECT 1
              FROM runner_current_session_placement AS current_placement
              JOIN runner_session_placement_record AS placement
                ON placement.session_id = current_placement.session_id
               AND placement.event_ordinal = current_placement.event_ordinal
              JOIN runner_enrollment AS lost_enrollment
                ON lost_enrollment.enrollment_id = NEW.enrollment_id
             WHERE (
                    placement.loss_fence_enrollment_id = NEW.enrollment_id
                    OR (
                        placement.loss_fence_enrollment_id IS NULL
                        AND placement.state_kind = 'unpinned'
                        AND placement.selector_kind = 'identity'
                        AND placement.selector_runner_id =
                            lost_enrollment.runner_id
                    )
               )
               AND (
                    placement.observed_runner_loss_epoch IS NULL
                    OR placement.observed_runner_loss_epoch < NEW.loss_epoch
               )
               AND (
                    placement.state_kind = 'pinned'
                    OR (
                        placement.state_kind = 'unpinned'
                        AND placement.selector_kind = 'identity'
                    )
               )
               AND placement.session_id <=
                    NEW.propagated_through_session_id
       )
    THEN
        RAISE EXCEPTION 'runner connection loss cursor skipped an affected session'
            USING ERRCODE = '23514';
    END IF;
    IF NEW.state_kind = 'completed'
       AND EXISTS (
            SELECT 1
              FROM runner_current_session_placement AS current_placement
              JOIN runner_session_placement_record AS placement
                ON placement.session_id = current_placement.session_id
               AND placement.event_ordinal = current_placement.event_ordinal
              JOIN runner_enrollment AS lost_enrollment
                ON lost_enrollment.enrollment_id = NEW.enrollment_id
             WHERE (
                    placement.loss_fence_enrollment_id = NEW.enrollment_id
                    OR (
                        placement.loss_fence_enrollment_id IS NULL
                        AND placement.state_kind = 'unpinned'
                        AND placement.selector_kind = 'identity'
                        AND placement.selector_runner_id =
                            lost_enrollment.runner_id
                    )
               )
               AND (
                    placement.observed_runner_loss_epoch IS NULL
                    OR placement.observed_runner_loss_epoch < NEW.loss_epoch
               )
               AND (
                    placement.state_kind = 'pinned'
                    OR (
                        placement.state_kind = 'unpinned'
                        AND placement.selector_kind = 'identity'
                    )
               )
       )
    THEN
        RAISE EXCEPTION 'runner connection loss cursor completed before propagation'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER runner_connection_loss_propagation_is_guarded
BEFORE INSERT OR UPDATE OR DELETE ON runner_connection_loss_propagation
FOR EACH ROW
EXECUTE FUNCTION guard_runner_connection_loss_propagation();

CREATE TRIGGER runner_connection_loss_propagation_rejects_truncate
BEFORE TRUNCATE ON runner_connection_loss_propagation
FOR EACH STATEMENT
EXECUTE FUNCTION reject_immutable_record_change();

CREATE FUNCTION assert_runner_connection_loss_has_propagation(
    checked_enrollment uuid,
    checked_loss_epoch numeric
)
RETURNS void
LANGUAGE plpgsql
AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM runner_connection_loss_epoch AS loss
          JOIN runner_connection_loss_propagation AS propagation
            ON propagation.enrollment_id = loss.enrollment_id
           AND propagation.loss_epoch = loss.loss_epoch
         WHERE loss.enrollment_id = checked_enrollment
           AND loss.loss_epoch = checked_loss_epoch
    )
    THEN
        RAISE EXCEPTION 'runner connection loss lacks propagation cursor'
            USING ERRCODE = '23514';
    END IF;
END;
$$;

CREATE FUNCTION require_runner_connection_loss_has_propagation()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    PERFORM assert_runner_connection_loss_has_propagation(
        NEW.enrollment_id,
        NEW.loss_epoch
    );
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER runner_connection_loss_has_propagation
AFTER INSERT ON runner_connection_loss_epoch
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_runner_connection_loss_has_propagation();
