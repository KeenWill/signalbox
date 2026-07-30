-- Serialize finding-event admission through a mutable current-event head.
-- Locking immutable finding roots alone cannot refresh an event-table snapshot
-- after a wait because terminalization appends an event rather than updating
-- the root.

-- Preserve the schema selected by the migration connection, while placing the
-- temporary schema after persistent and catalog objects for every unqualified
-- migration-time lookup.
DO $migration$
DECLARE
    workflow_schema name := pg_catalog.current_schema();
BEGIN
    IF workflow_schema IS NULL THEN
        RAISE EXCEPTION 'review workflow migration requires a current schema';
    END IF;
    PERFORM pg_catalog.set_config(
        'search_path',
        pg_catalog.format(
            '%I, pg_catalog, pg_temp',
            workflow_schema
        ),
        true
    );
END;
$migration$;

CREATE TABLE review_finding_event_head (
    finding_id uuid PRIMARY KEY,
    event_ordinal bigint,
    status text NOT NULL,
    event_pass_kind text,
    external_link_id uuid,

    CONSTRAINT review_finding_event_head_status_closed
        CHECK (
            status IN (
                'open',
                'accepted',
                'rejected',
                'duplicate',
                'superseded',
                'stale',
                'posted',
                'fixed',
                'blocked_with_reason'
            )
        ),
    CONSTRAINT review_finding_event_head_shape
        CHECK (
            (
                event_ordinal IS NULL
                AND status = 'open'
                AND event_pass_kind IS NULL
                AND external_link_id IS NULL
            )
            OR (
                event_ordinal IS NOT NULL
                AND event_ordinal BETWEEN 1 AND 4294967295
                AND status <> 'open'
                AND event_pass_kind IS NOT NULL
            )
        ),
    CONSTRAINT review_finding_event_head_finding_fk
        FOREIGN KEY (finding_id)
        REFERENCES review_finding (finding_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CONSTRAINT review_finding_event_head_event_fk
        FOREIGN KEY (finding_id, event_ordinal)
        REFERENCES review_finding_event (finding_id, event_ordinal)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

INSERT INTO review_finding_event_head (
    finding_id,
    event_ordinal,
    status,
    event_pass_kind,
    external_link_id
)
SELECT finding.finding_id,
       latest.event_ordinal,
       COALESCE(latest.event_kind, 'open'),
       event_pass.pass_kind,
       latest.external_link_id
  FROM review_finding AS finding
  LEFT JOIN LATERAL (
      SELECT event.event_ordinal,
             event.event_kind,
             event.event_pass_id,
             event.event_pass_run_id,
             event.target_id,
             event.external_link_id
        FROM review_finding_event AS event
       WHERE event.finding_id = finding.finding_id
       ORDER BY event.event_ordinal DESC
       LIMIT 1
  ) AS latest ON true
  LEFT JOIN review_pass AS event_pass
    ON event_pass.pass_id = latest.event_pass_id
   AND event_pass.run_id = latest.event_pass_run_id
   AND event_pass.target_id = latest.target_id;

CREATE FUNCTION create_review_finding_event_head()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    INSERT INTO review_finding_event_head (
        finding_id,
        event_ordinal,
        status,
        event_pass_kind,
        external_link_id
    )
    VALUES (NEW.finding_id, NULL, 'open', NULL, NULL);
    RETURN NULL;
END;
$$;

CREATE TRIGGER review_finding_event_head_is_created
AFTER INSERT ON review_finding
FOR EACH ROW
EXECUTE FUNCTION create_review_finding_event_head();

CREATE FUNCTION guard_review_finding_event_head_change()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'review finding event head is not deletable'
            USING ERRCODE = '23514';
    END IF;
    IF TG_OP = 'INSERT' THEN
        IF NEW.event_ordinal IS NOT NULL
           OR NEW.status <> 'open'
           OR NEW.event_pass_kind IS NOT NULL
           OR NEW.external_link_id IS NOT NULL
           OR EXISTS (
               SELECT 1
                 FROM review_finding_event
                WHERE finding_id = NEW.finding_id
           )
        THEN
            RAISE EXCEPTION 'review finding event head must begin open'
                USING ERRCODE = '23514';
        END IF;
        RETURN NEW;
    END IF;
    IF NEW.finding_id <> OLD.finding_id
       OR NEW.event_ordinal IS DISTINCT FROM
            COALESCE(OLD.event_ordinal, 0) + 1
       OR NEW.status = 'open'
       OR NEW.event_pass_kind IS NULL
       OR NOT EXISTS (
           SELECT 1
             FROM review_finding_event AS event
             JOIN review_pass AS event_pass
               ON event_pass.pass_id = event.event_pass_id
              AND event_pass.run_id = event.event_pass_run_id
              AND event_pass.target_id = event.target_id
            WHERE event.finding_id = NEW.finding_id
              AND event.event_ordinal = NEW.event_ordinal
              AND event.event_kind = NEW.status
              AND event_pass.pass_kind = NEW.event_pass_kind
              AND event.external_link_id
                    IS NOT DISTINCT FROM NEW.external_link_id
       )
    THEN
        RAISE EXCEPTION
            'review finding event head must advance to its exact durable event'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER review_finding_event_head_change_is_guarded
BEFORE INSERT OR UPDATE OR DELETE ON review_finding_event_head
FOR EACH ROW
EXECUTE FUNCTION guard_review_finding_event_head_change();

CREATE FUNCTION authenticate_review_finding_event_head()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    locked_head record;
    subject_ordinal bigint;
    subject_status text;
    subject_pass_kind text;
    subject_external_link uuid;
    referenced_status text;
BEGIN
    FOR locked_head IN
        SELECT head.finding_id,
               head.event_ordinal,
               head.status,
               head.event_pass_kind,
               head.external_link_id
          FROM review_finding_event_head AS head
         WHERE head.finding_id IN (
                   NEW.finding_id,
                   NEW.referenced_finding_id
               )
         ORDER BY head.finding_id
         FOR UPDATE
    LOOP
        IF locked_head.finding_id = NEW.finding_id THEN
            subject_ordinal := locked_head.event_ordinal;
            subject_status := locked_head.status;
            subject_pass_kind := locked_head.event_pass_kind;
            subject_external_link := locked_head.external_link_id;
        END IF;
        IF locked_head.finding_id = NEW.referenced_finding_id THEN
            referenced_status := locked_head.status;
        END IF;
    END LOOP;

    IF subject_status IS NULL THEN
        RAISE EXCEPTION 'finding event subject lacks its transition head'
            USING ERRCODE = '23514';
    END IF;
    IF NEW.event_ordinal IS DISTINCT FROM
        COALESCE(subject_ordinal, 0) + 1
    THEN
        RAISE EXCEPTION
            'finding event ordinal %, expected %',
            NEW.event_ordinal,
            COALESCE(subject_ordinal, 0) + 1
            USING ERRCODE = '23514';
    END IF;
    IF NEW.referenced_finding_id IS NOT NULL
       AND (
           referenced_status NOT IN ('open', 'accepted')
           OR NEW.referenced_finding_status
                IS DISTINCT FROM referenced_status
       )
    THEN
        RAISE EXCEPTION
            'referenced finding status % is not eligible or authenticated',
            referenced_status
            USING ERRCODE = '23514';
    END IF;
    IF NOT (
        (
            subject_status = 'open'
            AND NEW.event_kind IN (
                'accepted',
                'rejected',
                'duplicate',
                'superseded',
                'stale'
            )
        )
        OR (
            subject_status = 'accepted'
            AND NEW.event_kind IN (
                'duplicate',
                'superseded',
                'stale',
                'posted',
                'fixed',
                'blocked_with_reason'
            )
        )
        OR (
            subject_status = 'posted'
            AND NEW.event_kind IN (
                'superseded',
                'stale',
                'fixed',
                'blocked_with_reason'
            )
        )
        OR (
            subject_status = 'blocked_with_reason'
            AND (
                NEW.event_kind IN ('superseded', 'stale', 'fixed')
                OR (
                    NEW.event_kind = 'posted'
                    AND subject_pass_kind = 'publish'
                    AND NEW.external_link_id
                        IS NOT DISTINCT FROM subject_external_link
                )
            )
        )
    ) THEN
        RAISE EXCEPTION
            'invalid finding transition from % through %',
            subject_status,
            NEW.event_kind
            USING ERRCODE = '23514';
    END IF;

    RETURN NEW;
END;
$$;

-- This trigger intentionally sorts after
-- review_finding_event_sequence_is_guarded. The existing trigger locks
-- immutable finding roots first; the transition-head lock then follows one
-- global root-to-head order for store and direct relational admission alike.
CREATE TRIGGER review_finding_event_transition_head_is_guarded
BEFORE INSERT ON review_finding_event
FOR EACH ROW
EXECUTE FUNCTION authenticate_review_finding_event_head();

CREATE FUNCTION advance_review_finding_event_head()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    new_event_pass_kind text;
BEGIN
    SELECT pass_kind INTO new_event_pass_kind
      FROM review_pass
     WHERE pass_id = NEW.event_pass_id
       AND run_id = NEW.event_pass_run_id
       AND target_id = NEW.target_id;
    IF new_event_pass_kind IS NULL THEN
        RAISE EXCEPTION 'finding event pass is missing'
            USING ERRCODE = '23514';
    END IF;

    UPDATE review_finding_event_head
       SET event_ordinal = NEW.event_ordinal,
           status = NEW.event_kind,
           event_pass_kind = new_event_pass_kind,
           external_link_id = NEW.external_link_id
     WHERE finding_id = NEW.finding_id;
    RETURN NULL;
END;
$$;

CREATE TRIGGER review_finding_event_transition_head_is_advanced
AFTER INSERT ON review_finding_event
FOR EACH ROW
EXECUTE FUNCTION advance_review_finding_event_head();

-- The mutable head now exclusively authenticates the current ordinal,
-- referenced status, and subject transition after its lock wait. Retain the
-- legacy trigger's policy, producer, result, cycle, and external-link checks,
-- but remove its event-history reads because an INSERT statement snapshot can
-- predate the root-lock wait.
CREATE OR REPLACE FUNCTION require_review_finding_event_sequence()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    event_pass_kind text;
    event_pass_state text;
    event_pass_result_kind text;
    event_pass_result_finding uuid;
    event_pass_result_run uuid;
    event_pass_result_pass uuid;
    event_pass_result_ordinal bigint;
    event_pass_result_event_kind text;
    event_pass_result_reason text;
    event_pass_result_referenced_finding uuid;
    event_pass_result_referenced_run uuid;
    event_pass_result_referenced_target uuid;
    event_pass_result_referenced_pass uuid;
    event_pass_result_referenced_status text;
    event_pass_result_external_link uuid;
    event_policy_version bigint;
    event_judge_confidence integer;
    event_publication_confidence integer;
    finding_policy_version bigint;
    finding_judge_confidence integer;
    finding_publication_confidence integer;
    finding_is_real_confidence integer;
    finding_producing_pass uuid;
    referenced_pass_kind text;
    referenced_pass_state text;
    referenced_pass_result_kind text;
    referenced_run_state text;
    referenced_run_state_pass uuid;
    referenced_policy_version bigint;
    referenced_judge_confidence integer;
    referenced_publication_confidence integer;
    referenced_seal_count integer;
BEGIN
    PERFORM finding_id
      FROM review_finding
     WHERE finding_id IN (
         NEW.finding_id,
         NEW.referenced_finding_id
     )
     ORDER BY finding_id
     FOR NO KEY UPDATE;

    SELECT finding.is_real_confidence,
           finding.producing_pass_id,
           producing_run.policy_version,
           producing_run.minimum_judge_confidence,
           producing_run.minimum_publication_confidence
      INTO finding_is_real_confidence,
           finding_producing_pass,
           finding_policy_version,
           finding_judge_confidence,
           finding_publication_confidence
      FROM review_finding AS finding
      JOIN review_run AS producing_run
        ON producing_run.run_id = finding.run_id
       AND producing_run.target_id = finding.target_id
     WHERE finding.finding_id = NEW.finding_id
       AND finding.run_id = NEW.finding_run_id
       AND finding.target_id = NEW.target_id;

    SELECT pass.pass_kind, pass.state_kind,
           pass.result_kind,
           pass.result_finding_id,
           pass.result_finding_run_id,
           pass.result_finding_pass_id,
           pass.result_event_ordinal,
           pass.result_event_kind,
           pass.result_reason,
           pass.result_referenced_finding_id,
           pass.result_referenced_finding_run_id,
           pass.result_referenced_finding_target_id,
           pass.result_referenced_finding_pass_id,
           pass.result_referenced_finding_status,
           pass.result_external_link_id,
           event_run.policy_version,
           event_run.minimum_judge_confidence,
           event_run.minimum_publication_confidence
      INTO event_pass_kind, event_pass_state,
           event_pass_result_kind,
           event_pass_result_finding,
           event_pass_result_run,
           event_pass_result_pass,
           event_pass_result_ordinal,
           event_pass_result_event_kind,
           event_pass_result_reason,
           event_pass_result_referenced_finding,
           event_pass_result_referenced_run,
           event_pass_result_referenced_target,
           event_pass_result_referenced_pass,
           event_pass_result_referenced_status,
           event_pass_result_external_link,
           event_policy_version,
           event_judge_confidence,
           event_publication_confidence
      FROM review_pass AS pass
      JOIN review_run AS event_run
        ON event_run.run_id = pass.run_id
       AND event_run.target_id = pass.target_id
     WHERE pass.pass_id = NEW.event_pass_id
       AND pass.run_id = NEW.event_pass_run_id
       AND pass.target_id = NEW.target_id;

    IF event_policy_version IS DISTINCT FROM finding_policy_version
       OR event_judge_confidence IS DISTINCT FROM finding_judge_confidence
       OR event_publication_confidence
            IS DISTINCT FROM finding_publication_confidence
    THEN
        RAISE EXCEPTION
            'finding event pass policy differs from finding policy'
            USING ERRCODE = '23514';
    END IF;

    IF NEW.referenced_finding_id IS NOT NULL THEN
        SELECT referenced_pass.pass_kind,
               referenced_pass.state_kind,
               referenced_pass.result_kind,
               referenced_run.state_kind,
               referenced_run.state_pass_id,
               referenced_run.policy_version,
               referenced_run.minimum_judge_confidence,
               referenced_run.minimum_publication_confidence,
               seal.finding_count
          INTO referenced_pass_kind,
               referenced_pass_state,
               referenced_pass_result_kind,
               referenced_run_state,
               referenced_run_state_pass,
               referenced_policy_version,
               referenced_judge_confidence,
               referenced_publication_confidence,
               referenced_seal_count
          FROM review_finding AS referenced
          JOIN review_pass AS referenced_pass
            ON referenced_pass.pass_id =
                referenced.producing_pass_id
           AND referenced_pass.run_id = referenced.run_id
           AND referenced_pass.target_id = referenced.target_id
          JOIN review_run AS referenced_run
            ON referenced_run.run_id = referenced.run_id
           AND referenced_run.target_id = referenced.target_id
          LEFT JOIN review_pass_finding_inventory_seal AS seal
            ON seal.pass_id = referenced.producing_pass_id
         WHERE referenced.finding_id = NEW.referenced_finding_id
           AND referenced.run_id = NEW.referenced_finding_run_id
           AND referenced.target_id = NEW.referenced_finding_target_id
           AND referenced.producing_pass_id =
                NEW.referenced_finding_pass_id;

        IF NEW.referenced_finding_target_id
                IS DISTINCT FROM NEW.target_id
           OR referenced_pass_kind
                IS DISTINCT FROM 'read_only_review'
           OR referenced_pass_state IS DISTINCT FROM 'succeeded'
           OR referenced_pass_result_kind
                IS DISTINCT FROM 'produced_findings'
           OR referenced_run_state IS DISTINCT FROM 'succeeded'
           OR referenced_run_state_pass
                IS DISTINCT FROM NEW.referenced_finding_pass_id
           OR referenced_seal_count IS NULL
           OR NOT EXISTS (
               SELECT 1
                 FROM review_pass_produced_finding
                WHERE finding_id = NEW.referenced_finding_id
                  AND finding_run_id =
                        NEW.referenced_finding_run_id
                  AND target_id =
                        NEW.referenced_finding_target_id
                  AND finding_pass_id =
                        NEW.referenced_finding_pass_id
                  AND pass_id =
                        NEW.referenced_finding_pass_id
           )
        THEN
            RAISE EXCEPTION
                'referenced finding producer or sealed inventory is invalid'
                USING ERRCODE = '23514';
        END IF;

        IF referenced_policy_version
                IS DISTINCT FROM finding_policy_version
           OR referenced_judge_confidence
                IS DISTINCT FROM finding_judge_confidence
           OR referenced_publication_confidence
                IS DISTINCT FROM finding_publication_confidence
        THEN
            RAISE EXCEPTION
                'referenced finding policy differs from finding policy'
                USING ERRCODE = '23514';
        END IF;
    END IF;

    IF NEW.event_kind = 'accepted'
       AND finding_is_real_confidence < finding_judge_confidence
    THEN
        RAISE EXCEPTION
            'finding is-real confidence is below the judge threshold'
            USING ERRCODE = '23514';
    END IF;
    IF NEW.event_kind = 'posted'
       AND finding_is_real_confidence < finding_publication_confidence
    THEN
        RAISE EXCEPTION
            'finding is-real confidence is below the publication threshold'
            USING ERRCODE = '23514';
    END IF;

    IF event_pass_kind IS NULL
       OR NOT (
           (
               NEW.event_kind IN ('accepted', 'rejected', 'stale')
               AND event_pass_kind = 'judge'
           )
           OR (
               NEW.event_kind IN ('duplicate', 'superseded')
               AND event_pass_kind = 'dedupe'
           )
           OR (
               NEW.event_kind = 'posted'
               AND event_pass_kind IN (
                   'publish',
                   'import_external_context'
               )
           )
           OR (
               NEW.event_kind = 'fixed'
               AND event_pass_kind = 'fix'
           )
           OR (
               NEW.event_kind = 'blocked_with_reason'
               AND (
                   (
                       event_pass_kind = 'publish'
                       AND NEW.external_link_id IS NOT NULL
                   )
                   OR (
                       event_pass_kind = 'fix'
                       AND NEW.external_link_id IS NULL
                   )
               )
           )
       )
    THEN
        RAISE EXCEPTION
            'finding event % is incompatible with pass kind %',
            NEW.event_kind,
            event_pass_kind
            USING ERRCODE = '23514';
    END IF;

    IF (
        NEW.event_kind = 'blocked_with_reason'
        AND event_pass_state IS DISTINCT FROM 'blocked'
    ) OR (
        NEW.event_kind <> 'blocked_with_reason'
        AND event_pass_state IS DISTINCT FROM 'succeeded'
    )
    THEN
        RAISE EXCEPTION
            'finding event % is incompatible with pass state %',
            NEW.event_kind,
            event_pass_state
            USING ERRCODE = '23514';
    END IF;

    IF event_pass_result_kind IS DISTINCT FROM (
           CASE NEW.event_kind
               WHEN 'posted' THEN 'external_link_attachment'
               ELSE 'finding_event'
           END
       )
       OR event_pass_result_finding IS DISTINCT FROM NEW.finding_id
       OR event_pass_result_run IS DISTINCT FROM NEW.finding_run_id
       OR event_pass_result_pass IS DISTINCT FROM (
           SELECT producing_pass_id
             FROM review_finding
            WHERE finding_id = NEW.finding_id
              AND run_id = NEW.finding_run_id
              AND target_id = NEW.target_id
       )
       OR event_pass_result_ordinal IS DISTINCT FROM NEW.event_ordinal
       OR event_pass_result_event_kind IS DISTINCT FROM NEW.event_kind
       OR event_pass_result_reason IS DISTINCT FROM NEW.reason
       OR event_pass_result_referenced_finding
            IS DISTINCT FROM NEW.referenced_finding_id
       OR event_pass_result_referenced_run
            IS DISTINCT FROM NEW.referenced_finding_run_id
       OR event_pass_result_referenced_target
            IS DISTINCT FROM NEW.referenced_finding_target_id
       OR event_pass_result_referenced_pass
            IS DISTINCT FROM NEW.referenced_finding_pass_id
       OR event_pass_result_referenced_status
            IS DISTINCT FROM NEW.referenced_finding_status
       OR event_pass_result_external_link
            IS DISTINCT FROM NEW.external_link_id
    THEN
        RAISE EXCEPTION
            'finding event is not the exact result committed by its pass'
            USING ERRCODE = '23514';
    END IF;

    IF NEW.referenced_finding_id IS NOT NULL THEN
        IF EXISTS (
            WITH RECURSIVE referenced_ancestry(
                finding_id,
                run_id,
                target_id,
                pass_id
            ) AS (
                SELECT NEW.referenced_finding_id,
                       NEW.referenced_finding_run_id,
                       NEW.referenced_finding_target_id,
                       NEW.referenced_finding_pass_id
                UNION
                SELECT latest.referenced_finding_id,
                       latest.referenced_finding_run_id,
                       latest.referenced_finding_target_id,
                       latest.referenced_finding_pass_id
                  FROM referenced_ancestry AS ancestry
                  JOIN LATERAL (
                      SELECT referenced_finding_id,
                             referenced_finding_run_id,
                             referenced_finding_target_id,
                             referenced_finding_pass_id
                        FROM review_finding_event
                       WHERE finding_id = ancestry.finding_id
                         AND finding_run_id = ancestry.run_id
                         AND target_id = ancestry.target_id
                         AND event_kind IN (
                             'duplicate',
                             'superseded'
                         )
                       ORDER BY event_ordinal DESC
                       LIMIT 1
                  ) AS latest
                    ON latest.referenced_finding_id IS NOT NULL
            )
            SELECT 1
              FROM referenced_ancestry
             WHERE finding_id = NEW.finding_id
               AND run_id = NEW.finding_run_id
               AND target_id = NEW.target_id
               AND pass_id = finding_producing_pass
        )
        THEN
            RAISE EXCEPTION
                'finding reference would create a cycle'
                USING ERRCODE = '23514';
        END IF;
    END IF;

    IF NEW.event_kind = 'blocked_with_reason'
       AND NEW.external_link_id IS NOT NULL
       AND EXISTS (
           SELECT 1
             FROM review_external_link_attachment
            WHERE external_link_id = NEW.external_link_id
              AND target_id = NEW.target_id
       )
    THEN
        RAISE EXCEPTION
            'publication block requires a pending reservation'
            USING ERRCODE = '23514';
    END IF;

    IF NEW.event_kind = 'posted'
       AND EXISTS (
           SELECT 1
             FROM review_finding_event
            WHERE finding_id = NEW.finding_id
              AND event_kind = 'posted'
              AND external_link_id = NEW.external_link_id
       )
    THEN
        RAISE EXCEPTION
            'posted event reused consumed attachment evidence'
            USING ERRCODE = '23514';
    END IF;

    IF NEW.event_kind = 'posted'
       AND NOT EXISTS (
           SELECT 1
             FROM review_external_link_attachment AS attachment
             JOIN review_external_link AS link
               ON link.external_link_id = attachment.external_link_id
              AND link.target_id = attachment.target_id
            WHERE attachment.external_link_id = NEW.external_link_id
              AND attachment.target_id = NEW.target_id
              AND attachment.pass_run_id = NEW.event_pass_run_id
              AND attachment.pass_id = NEW.event_pass_id
              AND link.object_kind IN (
                  'review',
                  'review_thread',
                  'review_comment',
                  'change_request_comment'
              )
       )
    THEN
        RAISE EXCEPTION
            'posted event pass did not produce its attachment'
            USING ERRCODE = '23514';
    END IF;

    RETURN NEW;
END;
$$;

CREATE FUNCTION assert_review_finding_event_head_complete(
    checked_finding uuid
)
RETURNS void
LANGUAGE plpgsql
AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM review_finding AS finding
          JOIN review_finding_event_head AS head
            ON head.finding_id = finding.finding_id
          LEFT JOIN review_finding_event AS event
            ON event.finding_id = head.finding_id
           AND event.event_ordinal = head.event_ordinal
          LEFT JOIN review_pass AS event_pass
            ON event_pass.pass_id = event.event_pass_id
           AND event_pass.run_id = event.event_pass_run_id
           AND event_pass.target_id = event.target_id
         WHERE finding.finding_id = checked_finding
           AND (
               (
                   head.event_ordinal IS NULL
                   AND head.status = 'open'
                   AND head.event_pass_kind IS NULL
                   AND head.external_link_id IS NULL
                   AND NOT EXISTS (
                       SELECT 1
                         FROM review_finding_event AS existing
                        WHERE existing.finding_id = checked_finding
                   )
               )
               OR (
                   head.event_ordinal IS NOT NULL
                   AND event.event_kind = head.status
                   AND event_pass.pass_kind = head.event_pass_kind
                   AND event.external_link_id
                        IS NOT DISTINCT FROM head.external_link_id
                   AND head.event_ordinal = (
                       SELECT max(latest.event_ordinal)
                         FROM review_finding_event AS latest
                        WHERE latest.finding_id = checked_finding
                   )
                   AND head.event_ordinal = (
                       SELECT count(*)
                         FROM review_finding_event AS existing
                        WHERE existing.finding_id = checked_finding
                   )
               )
           )
    )
    THEN
        RAISE EXCEPTION
            'review finding event head does not name the exact latest event'
            USING ERRCODE = '23514';
    END IF;
END;
$$;

CREATE FUNCTION require_review_finding_event_head_complete()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    PERFORM assert_review_finding_event_head_complete(
        COALESCE(NEW.finding_id, OLD.finding_id)
    );
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER review_finding_requires_event_head
AFTER INSERT ON review_finding
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_review_finding_event_head_complete();

CREATE CONSTRAINT TRIGGER review_finding_event_rechecks_head
AFTER INSERT OR UPDATE OR DELETE ON review_finding_event
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_review_finding_event_head_complete();

CREATE CONSTRAINT TRIGGER review_finding_event_head_rechecks_finding
AFTER INSERT OR UPDATE OR DELETE ON review_finding_event_head
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_review_finding_event_head_complete();

CREATE TRIGGER review_finding_event_head_reject_truncate
BEFORE TRUNCATE ON review_finding_event_head
FOR EACH STATEMENT
EXECUTE FUNCTION reject_review_workflow_truncate();


-- Trigger functions retain the migration-selected persistent schema and name
-- pg_temp last, so later callers cannot shadow workflow relations with
-- session-local objects.
DO $migration$
DECLARE
    workflow_schema name := pg_catalog.current_schema();
BEGIN
    EXECUTE pg_catalog.format(
        'ALTER FUNCTION %I.create_review_finding_event_head() '
        'SET search_path TO %I, pg_catalog, pg_temp',
        workflow_schema,
        workflow_schema
    );
    EXECUTE pg_catalog.format(
        'ALTER FUNCTION %I.guard_review_finding_event_head_change() '
        'SET search_path TO %I, pg_catalog, pg_temp',
        workflow_schema,
        workflow_schema
    );
    EXECUTE pg_catalog.format(
        'ALTER FUNCTION %I.authenticate_review_finding_event_head() '
        'SET search_path TO %I, pg_catalog, pg_temp',
        workflow_schema,
        workflow_schema
    );
    EXECUTE pg_catalog.format(
        'ALTER FUNCTION %I.advance_review_finding_event_head() '
        'SET search_path TO %I, pg_catalog, pg_temp',
        workflow_schema,
        workflow_schema
    );
    EXECUTE pg_catalog.format(
        'ALTER FUNCTION %I.require_review_finding_event_sequence() '
        'SET search_path TO %I, pg_catalog, pg_temp',
        workflow_schema,
        workflow_schema
    );
    EXECUTE pg_catalog.format(
        'ALTER FUNCTION %I.assert_review_finding_event_head_complete(uuid) '
        'SET search_path TO %I, pg_catalog, pg_temp',
        workflow_schema,
        workflow_schema
    );
    EXECUTE pg_catalog.format(
        'ALTER FUNCTION %I.require_review_finding_event_head_complete() '
        'SET search_path TO %I, pg_catalog, pg_temp',
        workflow_schema,
        workflow_schema
    );
END;
$migration$;
