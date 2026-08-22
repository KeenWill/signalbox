-- Bind registration-triggered placement loss to the exact availability
-- revision that caused it, and make its per-session projection restartable.

DO $migration$
BEGIN
    IF EXISTS (
        SELECT 1
          FROM runner_session_placement_record
         WHERE loss_source_kind = 'registration'
    ) THEN
        RAISE EXCEPTION
            'runner registration loss cause cannot be inferred from legacy rows'
            USING ERRCODE = '23514';
    END IF;
END;
$migration$;

ALTER TABLE runner_session_placement_record
    ADD COLUMN loss_registration_revision numeric(20, 0),
    ADD CONSTRAINT runner_session_placement_loss_registration_positive
        CHECK (
            loss_registration_revision IS NULL
            OR loss_registration_revision BETWEEN 1 AND 18446744073709551615
        ),
    ADD CONSTRAINT runner_session_placement_loss_registration_shape
        CHECK (
            (
                loss_source_kind = 'registration'
                AND loss_registration_revision IS NOT NULL
            )
            OR (
                loss_source_kind IS DISTINCT FROM 'registration'
                AND loss_registration_revision IS NULL
            )
        ),
    ADD CONSTRAINT runner_session_placement_loss_registration_fk
        FOREIGN KEY (
            registration_enrollment_id,
            loss_registration_revision
        )
        REFERENCES runner_registration (
            enrollment_id,
            registration_revision
        )
        ON UPDATE RESTRICT ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED;

CREATE TABLE runner_registration_reconciliation (
    enrollment_id uuid NOT NULL,
    registration_revision numeric(20, 0) NOT NULL,
    propagated_through_session_id uuid,
    state_kind text NOT NULL,

    CONSTRAINT runner_registration_reconciliation_pk
        PRIMARY KEY (enrollment_id, registration_revision),
    CONSTRAINT runner_registration_reconciliation_state_closed
        CHECK (state_kind IN ('pending', 'completed')),
    CONSTRAINT runner_registration_reconciliation_registration_fk
        FOREIGN KEY (enrollment_id, registration_revision)
        REFERENCES runner_registration (enrollment_id, registration_revision)
        ON UPDATE RESTRICT ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT runner_registration_reconciliation_session_fk
        FOREIGN KEY (propagated_through_session_id)
        REFERENCES session (session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE TABLE runner_registration_reconciliation_observation (
    enrollment_id uuid NOT NULL,
    registration_revision numeric(20, 0) NOT NULL,
    session_id uuid NOT NULL,
    placement_event_ordinal numeric(20, 0) NOT NULL,
    disposition_kind text NOT NULL,

    CONSTRAINT runner_registration_reconciliation_observation_pk
        PRIMARY KEY (enrollment_id, registration_revision, session_id),
    CONSTRAINT runner_registration_reconciliation_observation_disposition_closed
        CHECK (
            disposition_kind IN ('preserved', 'runner_lost', 'superseded')
        ),
    CONSTRAINT runner_registration_reconciliation_observation_cursor_fk
        FOREIGN KEY (enrollment_id, registration_revision)
        REFERENCES runner_registration_reconciliation (
            enrollment_id,
            registration_revision
        )
        ON UPDATE RESTRICT ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT runner_registration_reconciliation_observation_placement_fk
        FOREIGN KEY (session_id, placement_event_ordinal)
        REFERENCES runner_session_placement_record (session_id, event_ordinal)
        ON UPDATE RESTRICT ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE FUNCTION runner_registration_preserves_placement(
    checked_enrollment uuid,
    checked_revision numeric,
    checked_session uuid,
    checked_event numeric
)
RETURNS boolean
LANGUAGE sql
STABLE
AS $function$
    SELECT COALESCE((
        SELECT registration.runner_id = placement.pinned_runner_id
           AND (
                (
                    placement.selector_kind = 'identity'
                    AND placement.selector_runner_id = registration.runner_id
                )
                OR (
                    placement.selector_kind = 'capability_class'
                    AND EXISTS (
                        SELECT 1
                          FROM runner_registration_class AS class
                         WHERE class.enrollment_id = checked_enrollment
                           AND class.registration_revision = checked_revision
                           AND class.capability_class =
                                placement.selector_capability_class
                    )
                )
           )
           AND EXISTS (
                SELECT 1
                  FROM runner_registration_sandbox AS sandbox
                 WHERE sandbox.enrollment_id = checked_enrollment
                   AND sandbox.registration_revision = checked_revision
                   AND sandbox.sandbox_profile =
                        placement.requested_sandbox_profile
           )
           AND NOT EXISTS (
                SELECT 1
                  FROM runner_session_placement_tool AS tool
                 WHERE tool.session_id = placement.session_id
                   AND tool.event_ordinal = placement.event_ordinal
                   AND tool.runner_required
                   AND NOT EXISTS (
                        SELECT 1
                          FROM runner_registration_tool AS advertised
                         WHERE advertised.enrollment_id = checked_enrollment
                           AND advertised.registration_revision = checked_revision
                           AND advertised.tool_name = tool.tool_name
                   )
           )
           AND (
                placement.requested_credential_profile_name IS NULL
                OR EXISTS (
                    SELECT 1
                      FROM runner_registration_profile AS profile
                     WHERE profile.enrollment_id = checked_enrollment
                       AND profile.registration_revision = checked_revision
                       AND profile.credential_profile_name =
                            placement.requested_credential_profile_name
                )
           )
           AND (
                placement.workspace_requirement_kind = 'none'
                OR (
                    placement.workspace_requirement_kind = 'repository_worktree'
                    AND EXISTS (
                        SELECT 1
                          FROM runner_registration_workspace AS workspace
                         WHERE workspace.enrollment_id = checked_enrollment
                           AND workspace.registration_revision = checked_revision
                           AND workspace.workspace_kind = 'worktree_per_session'
                    )
                    AND EXISTS (
                        SELECT 1
                          FROM runner_registration_repository AS repository
                         WHERE repository.enrollment_id = checked_enrollment
                           AND repository.registration_revision = checked_revision
                           AND repository.repository_key =
                                placement.requested_repository_key
                           AND repository.credential_profile_name IS NOT DISTINCT FROM
                                placement.requested_credential_profile_name
                    )
                )
           )
          FROM runner_session_placement_record AS placement
          JOIN runner_registration AS registration
            ON registration.enrollment_id = checked_enrollment
           AND registration.registration_revision = checked_revision
         WHERE placement.session_id = checked_session
           AND placement.event_ordinal = checked_event
           AND placement.state_kind = 'pinned'
           AND placement.registration_enrollment_id = checked_enrollment
    ), false);
$function$;

-- Historical revisions predate registration reconciliation. The current
-- revision starts pending so startup authenticates every still-pinned session
-- instead of guessing that earlier registration changes were harmless.
INSERT INTO runner_registration_reconciliation (
    enrollment_id,
    registration_revision,
    propagated_through_session_id,
    state_kind
)
SELECT registration.enrollment_id,
       registration.registration_revision,
       NULL,
       CASE
           WHEN current_registration.registration_revision =
                registration.registration_revision
               AND enrollment.state_kind = 'active'
           THEN 'pending'
           ELSE 'completed'
       END
  FROM runner_registration AS registration
  JOIN runner_current_registration AS current_registration
    ON current_registration.enrollment_id = registration.enrollment_id
  JOIN runner_enrollment AS enrollment
    ON enrollment.enrollment_id = registration.enrollment_id
 WHERE registration.registration_revision > 1;

CREATE FUNCTION guard_runner_registration_reconciliation_observation()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    placement runner_session_placement_record%ROWTYPE;
    predecessor runner_session_placement_record%ROWTYPE;
    predecessor_found boolean;
    current_event numeric;
    current_revision numeric;
BEGIN
    IF TG_OP <> 'INSERT' THEN
        RAISE EXCEPTION 'runner registration observation is immutable'
            USING ERRCODE = '23514';
    END IF;
    SELECT registration_revision INTO current_revision
      FROM runner_current_registration
     WHERE enrollment_id = NEW.enrollment_id;
    IF current_revision IS DISTINCT FROM NEW.registration_revision THEN
        RAISE EXCEPTION 'runner registration observation is not current'
            USING ERRCODE = '23514';
    END IF;
    SELECT event_ordinal INTO current_event
      FROM runner_current_session_placement
     WHERE session_id = NEW.session_id;
    IF current_event IS DISTINCT FROM NEW.placement_event_ordinal THEN
        RAISE EXCEPTION 'runner registration observation is not current placement'
            USING ERRCODE = '23514';
    END IF;
    SELECT * INTO placement
      FROM runner_session_placement_record
     WHERE session_id = NEW.session_id
       AND event_ordinal = NEW.placement_event_ordinal;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'runner registration observation lacks placement'
            USING ERRCODE = '23514';
    END IF;

    IF NEW.disposition_kind = 'preserved' THEN
        IF placement.state_kind <> 'pinned'
           OR placement.registration_enrollment_id IS DISTINCT FROM
                NEW.enrollment_id
           OR placement.registration_revision >= NEW.registration_revision
           OR NOT runner_registration_preserves_placement(
                NEW.enrollment_id,
                NEW.registration_revision,
                NEW.session_id,
                NEW.placement_event_ordinal
           )
        THEN
            RAISE EXCEPTION 'runner registration preservation is unauthenticated'
                USING ERRCODE = '23514';
        END IF;
    ELSIF NEW.disposition_kind = 'runner_lost' THEN
        SELECT * INTO predecessor
          FROM runner_session_placement_record
         WHERE session_id = NEW.session_id
           AND event_ordinal = NEW.placement_event_ordinal - 1;
        predecessor_found := FOUND;
        IF NOT predecessor_found
           OR placement.state_kind <> 'runner_lost'
           OR placement.event_kind <> 'runner_lost'
           OR placement.loss_source_kind <> 'registration'
           OR placement.loss_registration_revision IS DISTINCT FROM
                NEW.registration_revision
           OR predecessor.state_kind <> 'pinned'
           OR predecessor.registration_enrollment_id IS DISTINCT FROM
                NEW.enrollment_id
           OR predecessor.registration_revision >= NEW.registration_revision
           OR runner_registration_preserves_placement(
                NEW.enrollment_id,
                NEW.registration_revision,
                NEW.session_id,
                predecessor.event_ordinal
           )
        THEN
            RAISE EXCEPTION 'runner registration loss is unauthenticated'
                USING ERRCODE = '23514';
        END IF;
    ELSIF NEW.disposition_kind = 'superseded' THEN
        IF placement.state_kind = 'pinned'
           AND placement.registration_enrollment_id = NEW.enrollment_id
           AND placement.registration_revision < NEW.registration_revision
        THEN
            RAISE EXCEPTION 'runner registration candidate is not superseded'
                USING ERRCODE = '23514';
        END IF;
    ELSE
        RAISE EXCEPTION 'runner registration observation disposition is invalid'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE FUNCTION guard_runner_registration_loss_cause()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    predecessor runner_session_placement_record%ROWTYPE;
    predecessor_found boolean;
BEGIN
    IF NEW.loss_source_kind IS DISTINCT FROM 'registration' THEN
        IF NEW.loss_registration_revision IS NOT NULL THEN
            RAISE EXCEPTION 'non-registration loss carries registration cause'
                USING ERRCODE = '23514';
        END IF;
        RETURN NEW;
    END IF;
    SELECT * INTO predecessor
      FROM runner_session_placement_record
     WHERE session_id = NEW.session_id
       AND event_ordinal = NEW.event_ordinal - 1;
    predecessor_found := FOUND;
    IF NEW.event_kind = 'runner_lost' THEN
        IF NOT predecessor_found
           OR predecessor.state_kind <> 'pinned'
           OR predecessor.registration_enrollment_id IS DISTINCT FROM
                NEW.registration_enrollment_id
           OR NEW.loss_registration_revision IS NULL
           OR predecessor.registration_revision >=
                NEW.loss_registration_revision
           OR runner_registration_preserves_placement(
                NEW.registration_enrollment_id,
                NEW.loss_registration_revision,
                predecessor.session_id,
                predecessor.event_ordinal
           )
        THEN
            RAISE EXCEPTION 'runner registration loss cause is unauthenticated'
                USING ERRCODE = '23514';
        END IF;
    ELSIF NEW.event_kind = 'abandoned' THEN
        IF NOT predecessor_found
           OR predecessor.state_kind <> 'runner_lost'
           OR predecessor.loss_source_kind <> 'registration'
           OR predecessor.loss_registration_revision IS DISTINCT FROM
                NEW.loss_registration_revision
        THEN
            RAISE EXCEPTION 'abandonment changed runner registration loss cause'
                USING ERRCODE = '23514';
        END IF;
    ELSE
        RAISE EXCEPTION 'runner registration cause is attached to invalid event'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER runner_registration_loss_cause_is_guarded
BEFORE INSERT OR UPDATE ON runner_session_placement_record
FOR EACH ROW
EXECUTE FUNCTION guard_runner_registration_loss_cause();

CREATE FUNCTION assert_runner_registration_loss_has_observation(
    checked_session uuid,
    checked_event numeric
)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    placement runner_session_placement_record%ROWTYPE;
BEGIN
    SELECT * INTO placement
      FROM runner_session_placement_record
     WHERE session_id = checked_session
       AND event_ordinal = checked_event;
    IF FOUND
       AND placement.event_kind = 'runner_lost'
       AND placement.loss_source_kind = 'registration'
       AND NOT EXISTS (
            SELECT 1
              FROM runner_registration_reconciliation_observation AS observed
             WHERE observed.enrollment_id =
                    placement.registration_enrollment_id
               AND observed.registration_revision =
                    placement.loss_registration_revision
               AND observed.session_id = placement.session_id
               AND observed.placement_event_ordinal = placement.event_ordinal
               AND observed.disposition_kind = 'runner_lost'
       )
    THEN
        RAISE EXCEPTION 'runner registration loss lacks reconciliation observation'
            USING ERRCODE = '23514';
    END IF;
END;
$$;

CREATE FUNCTION require_runner_registration_loss_has_observation()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    PERFORM assert_runner_registration_loss_has_observation(
        NEW.session_id,
        NEW.event_ordinal
    );
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER runner_registration_loss_has_observation
AFTER INSERT OR UPDATE ON runner_session_placement_record
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_runner_registration_loss_has_observation();

CREATE TRIGGER runner_registration_reconciliation_observation_is_guarded
BEFORE INSERT OR UPDATE OR DELETE
ON runner_registration_reconciliation_observation
FOR EACH ROW
EXECUTE FUNCTION guard_runner_registration_reconciliation_observation();

CREATE TRIGGER runner_registration_reconciliation_observation_rejects_truncate
BEFORE TRUNCATE ON runner_registration_reconciliation_observation
FOR EACH STATEMENT
EXECUTE FUNCTION reject_immutable_record_change();

CREATE FUNCTION guard_runner_registration_reconciliation()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'runner registration reconciliation is durable'
            USING ERRCODE = '23514';
    END IF;
    IF TG_OP = 'INSERT' THEN
        IF NEW.registration_revision <= 1
           OR NEW.state_kind <> 'pending'
           OR NEW.propagated_through_session_id IS NOT NULL
           OR EXISTS (
                SELECT 1
                  FROM runner_registration_reconciliation AS prior
                 WHERE prior.enrollment_id = NEW.enrollment_id
                   AND prior.state_kind = 'pending'
           )
        THEN
            RAISE EXCEPTION 'new runner registration reconciliation must start pending'
                USING ERRCODE = '23514';
        END IF;
        RETURN NEW;
    END IF;
    IF NEW.enrollment_id IS DISTINCT FROM OLD.enrollment_id
       OR NEW.registration_revision IS DISTINCT FROM OLD.registration_revision
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
        RAISE EXCEPTION 'runner registration reconciliation must advance monotonically'
            USING ERRCODE = '23514';
    END IF;
    IF NEW.state_kind = 'pending'
       AND EXISTS (
            SELECT 1
              FROM runner_current_session_placement AS current_placement
              JOIN runner_session_placement_record AS placement
                ON placement.session_id = current_placement.session_id
               AND placement.event_ordinal = current_placement.event_ordinal
              LEFT JOIN runner_registration_reconciliation_observation AS observed
                ON observed.enrollment_id = NEW.enrollment_id
               AND observed.registration_revision = NEW.registration_revision
               AND observed.session_id = placement.session_id
             WHERE placement.state_kind = 'pinned'
               AND placement.registration_enrollment_id = NEW.enrollment_id
               AND placement.registration_revision < NEW.registration_revision
               AND placement.session_id <= NEW.propagated_through_session_id
               AND observed.session_id IS NULL
       )
    THEN
        RAISE EXCEPTION 'runner registration cursor skipped a pinned session'
            USING ERRCODE = '23514';
    END IF;
    IF NEW.state_kind = 'completed'
       AND EXISTS (
            SELECT 1
              FROM runner_current_session_placement AS current_placement
              JOIN runner_session_placement_record AS placement
                ON placement.session_id = current_placement.session_id
               AND placement.event_ordinal = current_placement.event_ordinal
              LEFT JOIN runner_registration_reconciliation_observation AS observed
                ON observed.enrollment_id = NEW.enrollment_id
               AND observed.registration_revision = NEW.registration_revision
               AND observed.session_id = placement.session_id
             WHERE placement.state_kind = 'pinned'
               AND placement.registration_enrollment_id = NEW.enrollment_id
               AND placement.registration_revision < NEW.registration_revision
               AND observed.session_id IS NULL
       )
    THEN
        RAISE EXCEPTION 'runner registration reconciliation completed before projection'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER runner_registration_reconciliation_is_guarded
BEFORE INSERT OR UPDATE OR DELETE ON runner_registration_reconciliation
FOR EACH ROW
EXECUTE FUNCTION guard_runner_registration_reconciliation();

CREATE TRIGGER runner_registration_reconciliation_rejects_truncate
BEFORE TRUNCATE ON runner_registration_reconciliation
FOR EACH STATEMENT
EXECUTE FUNCTION reject_immutable_record_change();

CREATE FUNCTION assert_runner_registration_has_reconciliation(
    checked_enrollment uuid,
    checked_revision numeric
)
RETURNS void
LANGUAGE plpgsql
AS $$
BEGIN
    IF checked_revision > 1 AND NOT EXISTS (
        SELECT 1
          FROM runner_registration_reconciliation
         WHERE enrollment_id = checked_enrollment
           AND registration_revision = checked_revision
    ) THEN
        RAISE EXCEPTION 'runner registration lacks reconciliation cursor'
            USING ERRCODE = '23514';
    END IF;
END;
$$;

CREATE FUNCTION require_runner_registration_has_reconciliation()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    PERFORM assert_runner_registration_has_reconciliation(
        NEW.enrollment_id,
        NEW.registration_revision
    );
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER runner_registration_has_reconciliation
AFTER INSERT ON runner_registration
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_runner_registration_has_reconciliation();
