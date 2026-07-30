-- Serialize finding-event admission through a mutable current-event head.
-- Locking immutable finding roots alone cannot refresh an event-table snapshot
-- after a wait because terminalization appends an event rather than updating
-- the root.

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
    THEN
        RAISE EXCEPTION 'review finding event head must advance exactly once'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER review_finding_event_head_change_is_guarded
BEFORE INSERT OR UPDATE OR DELETE ON review_finding_event_head
FOR EACH ROW
EXECUTE FUNCTION guard_review_finding_event_head_change();

CREATE FUNCTION advance_review_finding_event_head()
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
    new_event_pass_kind text;
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
EXECUTE FUNCTION advance_review_finding_event_head();

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
