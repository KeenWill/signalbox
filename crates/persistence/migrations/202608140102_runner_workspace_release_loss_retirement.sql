-- Retire one pending managed-workspace release when its cleanup-owning
-- physical connection becomes durably lost. The immutable release remains as
-- recorded-leak evidence; this terminal proof removes it from redelivery.

CREATE TABLE runner_workspace_release_loss_retirement (
    session_id uuid NOT NULL,
    placement_revision numeric(20, 0) NOT NULL,
    runner_id uuid NOT NULL,
    manifest_id uuid NOT NULL,
    enrollment_id uuid NOT NULL,
    connection_epoch numeric(20, 0) NOT NULL,
    loss_epoch numeric(20, 0) NOT NULL,
    connection_event_ordinal numeric(20, 0) NOT NULL,

    CONSTRAINT runner_workspace_release_loss_retirement_pk
        PRIMARY KEY (session_id, placement_revision),
    CONSTRAINT runner_workspace_release_loss_retirement_positive_u64 CHECK (
        placement_revision BETWEEN 1 AND 18446744073709551615
        AND connection_epoch BETWEEN 1 AND 18446744073709551615
        AND loss_epoch BETWEEN 1 AND 18446744073709551615
        AND connection_event_ordinal BETWEEN 1 AND 18446744073709551615
    ),
    CONSTRAINT runner_workspace_release_loss_retirement_release_fk
        FOREIGN KEY (
            session_id,
            placement_revision,
            runner_id,
            manifest_id
        )
        REFERENCES runner_workspace_release (
            session_id,
            placement_revision,
            runner_id,
            manifest_id
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT runner_workspace_release_loss_retirement_loss_fk
        FOREIGN KEY (enrollment_id, loss_epoch)
        REFERENCES runner_connection_loss_epoch (enrollment_id, loss_epoch)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE FUNCTION require_runner_workspace_release_loss_retirement_authority()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    release runner_workspace_release%ROWTYPE;
    loss runner_connection_loss_epoch%ROWTYPE;
BEGIN
    SELECT * INTO release
      FROM runner_workspace_release
     WHERE session_id = NEW.session_id
       AND placement_revision = NEW.placement_revision;
    SELECT * INTO loss
      FROM runner_connection_loss_epoch
     WHERE enrollment_id = NEW.enrollment_id
       AND loss_epoch = NEW.loss_epoch;

    IF release.state_kind IS DISTINCT FROM 'pending'
       OR release.runner_id IS DISTINCT FROM NEW.runner_id
       OR release.manifest_id IS DISTINCT FROM NEW.manifest_id
       OR release.enrollment_id IS DISTINCT FROM NEW.enrollment_id
       OR release.connection_epoch IS DISTINCT FROM NEW.connection_epoch
       OR loss.connection_epoch IS DISTINCT FROM NEW.connection_epoch
       OR loss.connection_event_ordinal IS DISTINCT FROM
            NEW.connection_event_ordinal
       OR EXISTS (
            SELECT 1
              FROM runner_workspace_release_acknowledgement AS acknowledgement
             WHERE acknowledgement.session_id = NEW.session_id
               AND acknowledgement.placement_revision =
                    NEW.placement_revision
       )
    THEN
        RAISE EXCEPTION 'workspace release loss retirement lacks exact pending authority'
            USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER runner_workspace_release_loss_retirement_is_checked
AFTER INSERT ON runner_workspace_release_loss_retirement
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_runner_workspace_release_loss_retirement_authority();

CREATE TRIGGER runner_workspace_release_loss_retirement_is_append_only
BEFORE UPDATE OR DELETE ON runner_workspace_release_loss_retirement
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER runner_workspace_release_loss_retirement_rejects_truncate
BEFORE TRUNCATE ON runner_workspace_release_loss_retirement
FOR EACH STATEMENT
EXECUTE FUNCTION reject_immutable_record_change();

-- No production writer predates this migration, but preserve exact valid rows
-- created by tests or a forward-compatible adapter: a release whose source
-- connection is already lost is deterministically unowned.
INSERT INTO runner_workspace_release_loss_retirement (
    session_id,
    placement_revision,
    runner_id,
    manifest_id,
    enrollment_id,
    connection_epoch,
    loss_epoch,
    connection_event_ordinal
)
SELECT release.session_id,
       release.placement_revision,
       release.runner_id,
       release.manifest_id,
       release.enrollment_id,
       release.connection_epoch,
       loss.loss_epoch,
       loss.connection_event_ordinal
  FROM runner_workspace_release AS release
  JOIN runner_connection_loss_epoch AS loss
    ON loss.enrollment_id = release.enrollment_id
   AND loss.connection_epoch = release.connection_epoch
  LEFT JOIN runner_workspace_release_acknowledgement AS acknowledgement
    ON acknowledgement.session_id = release.session_id
   AND acknowledgement.placement_revision = release.placement_revision
 WHERE acknowledgement.session_id IS NULL;

-- A release can outlive the placement that originally made its session part of
-- a loss page. Extend the cursor guard with that independent subject set so a
-- later placement change cannot hide an unowned release behind the cursor.
CREATE FUNCTION guard_runner_connection_loss_release_propagation()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF (
        NEW.state_kind = 'completed'
        OR (
            NEW.state_kind = 'pending'
            AND NEW.propagated_through_session_id IS NOT NULL
        )
    )
       AND EXISTS (
            SELECT 1
              FROM runner_workspace_release AS release
              JOIN runner_connection_loss_epoch AS loss
                ON loss.enrollment_id = NEW.enrollment_id
               AND loss.loss_epoch = NEW.loss_epoch
              LEFT JOIN runner_workspace_release_acknowledgement AS acknowledgement
                ON acknowledgement.session_id = release.session_id
               AND acknowledgement.placement_revision =
                    release.placement_revision
              LEFT JOIN runner_workspace_release_loss_retirement AS retirement
                ON retirement.session_id = release.session_id
               AND retirement.placement_revision = release.placement_revision
             WHERE release.enrollment_id = NEW.enrollment_id
               AND release.connection_epoch = loss.connection_epoch
               AND acknowledgement.session_id IS NULL
               AND retirement.session_id IS NULL
               AND (
                    NEW.state_kind = 'completed'
                    OR release.session_id <=
                        NEW.propagated_through_session_id
               )
       )
    THEN
        RAISE EXCEPTION 'runner connection loss cursor skipped a pending workspace release'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER runner_connection_loss_propagation_release_is_guarded
BEFORE INSERT OR UPDATE ON runner_connection_loss_propagation
FOR EACH ROW
EXECUTE FUNCTION guard_runner_connection_loss_release_propagation();

-- Completion and loss are mutually exclusive terminal proofs. The application
-- transactions serialize on the release row; the relational guard also keeps
-- direct or future writers from committing both.
CREATE OR REPLACE FUNCTION require_runner_workspace_release_acknowledgement_authority()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    release runner_workspace_release%ROWTYPE;
BEGIN
    SELECT * INTO release
      FROM runner_workspace_release
     WHERE session_id = NEW.session_id
       AND placement_revision = NEW.placement_revision;

    IF release.state_kind IS DISTINCT FROM 'pending'
       OR release.runner_id IS DISTINCT FROM NEW.runner_id
       OR release.manifest_id IS DISTINCT FROM NEW.manifest_id
       OR EXISTS (
            SELECT 1
              FROM runner_connection_loss_epoch AS loss
             WHERE loss.enrollment_id = release.enrollment_id
               AND loss.connection_epoch = release.connection_epoch
       )
       OR EXISTS (
            SELECT 1
              FROM runner_workspace_release_loss_retirement AS retirement
             WHERE retirement.session_id = NEW.session_id
               AND retirement.placement_revision = NEW.placement_revision
       )
    THEN
        RAISE EXCEPTION 'workspace release acknowledgement lacks live pending authority'
            USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;
