-- Authenticate duplicate and superseded references across producer runs while
-- retaining each finding's complete immutable ancestry.

ALTER TABLE review_pass
    ADD COLUMN result_referenced_finding_target_id uuid;

UPDATE review_pass
   SET result_referenced_finding_target_id = target_id
 WHERE result_referenced_finding_id IS NOT NULL;

ALTER TABLE review_pass
    DROP CONSTRAINT review_pass_result_referenced_finding_fk,
    ADD CONSTRAINT review_pass_result_referenced_target_shape
        CHECK (
            (
                result_referenced_finding_id IS NULL
                AND result_referenced_finding_target_id IS NULL
            )
            OR (
                result_referenced_finding_id IS NOT NULL
                AND result_referenced_finding_target_id IS NOT NULL
                AND result_referenced_finding_target_id = target_id
            )
        ),
    ADD CONSTRAINT review_pass_result_referenced_finding_fk
        FOREIGN KEY (
            result_referenced_finding_id,
            result_referenced_finding_run_id,
            result_referenced_finding_target_id,
            result_referenced_finding_pass_id
        )
        REFERENCES review_finding (
            finding_id,
            run_id,
            target_id,
            producing_pass_id
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED;

ALTER TABLE review_pass_produced_finding
    ADD CONSTRAINT review_pass_produced_finding_complete_identity_key
        UNIQUE (
            finding_id,
            finding_run_id,
            target_id,
            finding_pass_id
        );

ALTER TABLE review_pass
    ADD CONSTRAINT review_pass_result_referenced_inventory_fk
        FOREIGN KEY (
            result_referenced_finding_id,
            result_referenced_finding_run_id,
            result_referenced_finding_target_id,
            result_referenced_finding_pass_id
        )
        REFERENCES review_pass_produced_finding (
            finding_id,
            finding_run_id,
            target_id,
            finding_pass_id
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED;

CREATE FUNCTION guard_bound_review_pass_referenced_target()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF OLD.result_kind IS NOT NULL
       AND NEW.result_referenced_finding_target_id
            IS DISTINCT FROM OLD.result_referenced_finding_target_id
    THEN
        RAISE EXCEPTION 'bound review pass result cannot change'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER review_pass_bound_referenced_target_is_guarded
BEFORE UPDATE OF result_referenced_finding_target_id ON review_pass
FOR EACH ROW
EXECUTE FUNCTION guard_bound_review_pass_referenced_target();

ALTER TABLE review_finding_event
    DISABLE TRIGGER review_finding_event_is_append_only,
    ADD COLUMN referenced_finding_run_id uuid,
    ADD COLUMN referenced_finding_target_id uuid,
    ADD COLUMN referenced_finding_pass_id uuid;

UPDATE review_finding_event AS event
   SET referenced_finding_run_id = referenced.run_id,
       referenced_finding_target_id = referenced.target_id,
       referenced_finding_pass_id = referenced.producing_pass_id
  FROM review_finding AS referenced
 WHERE referenced.finding_id = event.referenced_finding_id;

ALTER TABLE review_finding_event
    ENABLE TRIGGER review_finding_event_is_append_only,
    DROP CONSTRAINT review_finding_event_referenced_finding_fk,
    ADD CONSTRAINT review_finding_event_referenced_ancestry_shape
        CHECK (
            (
                referenced_finding_id IS NULL
                AND referenced_finding_run_id IS NULL
                AND referenced_finding_target_id IS NULL
                AND referenced_finding_pass_id IS NULL
            )
            OR (
                referenced_finding_id IS NOT NULL
                AND referenced_finding_run_id IS NOT NULL
                AND referenced_finding_target_id IS NOT NULL
                AND referenced_finding_pass_id IS NOT NULL
                AND referenced_finding_target_id = target_id
            )
        ),
    ADD CONSTRAINT review_finding_event_referenced_finding_fk
        FOREIGN KEY (
            referenced_finding_id,
            referenced_finding_run_id,
            referenced_finding_target_id,
            referenced_finding_pass_id
        )
        REFERENCES review_finding (
            finding_id,
            run_id,
            target_id,
            producing_pass_id
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    ADD CONSTRAINT review_finding_event_referenced_inventory_fk
        FOREIGN KEY (
            referenced_finding_id,
            referenced_finding_run_id,
            referenced_finding_target_id,
            referenced_finding_pass_id
        )
        REFERENCES review_pass_produced_finding (
            finding_id,
            finding_run_id,
            target_id,
            finding_pass_id
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED;

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
    previous_kind text;
    previous_pass_kind text;
    previous_external_link uuid;
    previous_status text;
    referenced_status text;
    expected_ordinal bigint;
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

    SELECT event.event_kind, pass.pass_kind, event.external_link_id
      INTO previous_kind, previous_pass_kind, previous_external_link
      FROM review_finding_event AS event
      JOIN review_pass AS pass
        ON pass.pass_id = event.event_pass_id
       AND pass.run_id = event.event_pass_run_id
       AND pass.target_id = event.target_id
     WHERE event.finding_id = NEW.finding_id
     ORDER BY event.event_ordinal DESC
     LIMIT 1;

    SELECT COALESCE(max(event_ordinal), 0) + 1
      INTO expected_ordinal
      FROM review_finding_event
     WHERE finding_id = NEW.finding_id;

    IF NEW.event_ordinal <> expected_ordinal THEN
        RAISE EXCEPTION
            'finding event ordinal %, expected %',
            NEW.event_ordinal,
            expected_ordinal
            USING ERRCODE = '23514';
    END IF;

    IF NEW.referenced_finding_id IS NOT NULL THEN
        SELECT CASE latest.event_kind
                   WHEN 'accepted' THEN 'accepted'
                   WHEN 'rejected' THEN 'rejected'
                   WHEN 'duplicate' THEN 'duplicate'
                   WHEN 'superseded' THEN 'superseded'
                   WHEN 'stale' THEN 'stale'
                   WHEN 'posted' THEN 'posted'
                   WHEN 'fixed' THEN 'fixed'
                   WHEN 'blocked_with_reason' THEN 'blocked_with_reason'
                   ELSE 'open'
               END
          INTO referenced_status
          FROM review_finding AS referenced
          LEFT JOIN LATERAL (
              SELECT event_kind
                FROM review_finding_event
               WHERE finding_id = referenced.finding_id
               ORDER BY event_ordinal DESC
               LIMIT 1
          ) AS latest ON true
         WHERE referenced.finding_id = NEW.referenced_finding_id
           AND referenced.run_id = NEW.referenced_finding_run_id
           AND referenced.target_id = NEW.referenced_finding_target_id
           AND referenced.producing_pass_id =
                NEW.referenced_finding_pass_id;

        IF referenced_status NOT IN ('open', 'accepted')
           OR NEW.referenced_finding_status
                IS DISTINCT FROM referenced_status
        THEN
            RAISE EXCEPTION
                'referenced finding status % is not eligible or authenticated',
                referenced_status
                USING ERRCODE = '23514';
        END IF;

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

    previous_status := CASE previous_kind
        WHEN 'accepted' THEN 'accepted'
        WHEN 'rejected' THEN 'rejected'
        WHEN 'duplicate' THEN 'duplicate'
        WHEN 'superseded' THEN 'superseded'
        WHEN 'stale' THEN 'stale'
        WHEN 'posted' THEN 'posted'
        WHEN 'fixed' THEN 'fixed'
        WHEN 'blocked_with_reason' THEN 'blocked_with_reason'
        ELSE 'open'
    END;

    IF NOT (
        (
            previous_status = 'open'
            AND NEW.event_kind IN (
                'accepted',
                'rejected',
                'duplicate',
                'superseded',
                'stale'
            )
        )
        OR (
            previous_status = 'accepted'
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
            previous_status = 'posted'
            AND NEW.event_kind IN (
                'superseded',
                'stale',
                'fixed',
                'blocked_with_reason'
            )
        )
        OR (
            previous_status = 'blocked_with_reason'
            AND (
                NEW.event_kind IN ('superseded', 'stale', 'fixed')
                OR (
                    NEW.event_kind = 'posted'
                    AND previous_pass_kind = 'publish'
                    AND NEW.external_link_id
                        IS NOT DISTINCT FROM previous_external_link
                )
            )
        )
    ) THEN
        RAISE EXCEPTION
            'invalid finding transition from % through %',
            previous_status,
            NEW.event_kind
            USING ERRCODE = '23514';
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

CREATE OR REPLACE FUNCTION require_review_pass_external_result()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.result_kind = 'finding_event'
       AND NOT EXISTS (
           SELECT 1
             FROM review_finding_event AS event
             JOIN review_finding AS finding
               ON finding.finding_id = event.finding_id
              AND finding.run_id = event.finding_run_id
              AND finding.target_id = event.target_id
            WHERE event.finding_id = NEW.result_finding_id
              AND event.finding_run_id = NEW.result_finding_run_id
              AND finding.producing_pass_id =
                    NEW.result_finding_pass_id
              AND event.target_id = NEW.target_id
              AND event.event_pass_id = NEW.pass_id
              AND event.event_pass_run_id = NEW.run_id
              AND event.event_ordinal = NEW.result_event_ordinal
              AND event.event_kind = NEW.result_event_kind
              AND event.reason IS NOT DISTINCT FROM NEW.result_reason
              AND event.referenced_finding_id IS NOT DISTINCT FROM
                    NEW.result_referenced_finding_id
              AND event.referenced_finding_run_id IS NOT DISTINCT FROM
                    NEW.result_referenced_finding_run_id
              AND event.referenced_finding_target_id IS NOT DISTINCT FROM
                    NEW.result_referenced_finding_target_id
              AND event.referenced_finding_pass_id IS NOT DISTINCT FROM
                    NEW.result_referenced_finding_pass_id
              AND event.referenced_finding_status IS NOT DISTINCT FROM
                    NEW.result_referenced_finding_status
              AND event.external_link_id IS NOT DISTINCT FROM
                    NEW.result_external_link_id
              AND event.external_link_association_kind IS NOT DISTINCT FROM
                    CASE
                        WHEN NEW.result_external_link_id IS NULL THEN NULL
                        ELSE 'finding'
                    END
       )
    THEN
        RAISE EXCEPTION
            'finding-event result omitted its exact child row'
            USING ERRCODE = '23514';
    END IF;
    IF NEW.result_kind = 'external_link_attachment'
       AND NOT EXISTS (
           SELECT 1
             FROM review_external_link_attachment
            WHERE external_link_id = NEW.result_external_link_id
              AND target_id = NEW.target_id
              AND pass_run_id = NEW.run_id
              AND pass_id = NEW.pass_id
              AND external_object_key =
                    NEW.result_external_object_key
       )
    THEN
        RAISE EXCEPTION
            'external attachment result omitted its exact child row'
            USING ERRCODE = '23514';
    END IF;
    IF NEW.result_kind = 'external_link_observation'
       AND NOT EXISTS (
           SELECT 1
             FROM review_external_link_observation
            WHERE external_link_id = NEW.result_external_link_id
              AND observation_ordinal = NEW.result_event_ordinal
              AND target_id = NEW.target_id
              AND pass_run_id = NEW.run_id
              AND pass_id = NEW.pass_id
              AND object_state =
                    NEW.result_observation_state
       )
    THEN
        RAISE EXCEPTION
            'external observation result omitted its exact child row'
            USING ERRCODE = '23514';
    END IF;
    IF NEW.result_kind = 'external_link_no_change'
       AND (
           NOT EXISTS (
               SELECT 1
                 FROM review_external_link_attachment
                WHERE external_link_id = NEW.result_external_link_id
                  AND target_id = NEW.target_id
           )
           OR NEW.result_observation_state IS DISTINCT FROM (
               SELECT object_state
                 FROM review_external_link_observation
                WHERE external_link_id = NEW.result_external_link_id
                ORDER BY observation_ordinal DESC
                LIMIT 1
           )
           OR NEW.result_event_ordinal IS DISTINCT FROM (
               SELECT observation_ordinal
                 FROM review_external_link_observation
                WHERE external_link_id = NEW.result_external_link_id
                ORDER BY observation_ordinal DESC
                LIMIT 1
           )
       )
    THEN
        RAISE EXCEPTION
            'unchanged external result differs from latest durable state'
            USING ERRCODE = '23514';
    END IF;
    IF NEW.result_kind = 'external_link_publication_blocked' THEN
        PERFORM 1
          FROM review_external_link
         WHERE external_link_id = NEW.result_external_link_id
         FOR NO KEY UPDATE;
    END IF;
    IF NEW.result_kind = 'external_link_publication_blocked'
       AND EXISTS (
           SELECT 1
             FROM review_external_link_attachment
            WHERE external_link_id = NEW.result_external_link_id
              AND target_id = NEW.target_id
       )
    THEN
        RAISE EXCEPTION
            'blocked publication result requires a pending reservation'
            USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;
