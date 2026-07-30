-- Split review-finding confidence into independent is-real and severity-label axes.
-- Existing pre-deployment rows preserve their former score on both axes.

ALTER TABLE review_finding
    DISABLE TRIGGER review_finding_is_append_only,
    ADD COLUMN severity_label_confidence integer;

UPDATE review_finding
   SET severity_label_confidence = confidence;

ALTER TABLE review_finding
    RENAME COLUMN confidence TO is_real_confidence;

ALTER TABLE review_finding
    DROP CONSTRAINT review_finding_confidence_bounds,
    ALTER COLUMN severity_label_confidence SET NOT NULL,
    ADD CONSTRAINT review_finding_is_real_confidence_bounds
        CHECK (is_real_confidence BETWEEN 0 AND 10000),
    ADD CONSTRAINT review_finding_severity_label_confidence_bounds
        CHECK (severity_label_confidence BETWEEN 0 AND 10000),
    ENABLE TRIGGER review_finding_is_append_only;

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
           producing_run.policy_version,
           producing_run.minimum_judge_confidence,
           producing_run.minimum_publication_confidence
      INTO finding_is_real_confidence,
           finding_policy_version,
           finding_judge_confidence,
           finding_publication_confidence
      FROM review_finding AS finding
      JOIN review_run AS producing_run
        ON producing_run.run_id = finding.run_id
       AND producing_run.target_id = finding.target_id
     WHERE finding.finding_id = NEW.finding_id
    ;

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
       )
       OR event_pass_result_ordinal IS DISTINCT FROM NEW.event_ordinal
       OR event_pass_result_event_kind IS DISTINCT FROM NEW.event_kind
       OR event_pass_result_reason IS DISTINCT FROM NEW.reason
       OR event_pass_result_referenced_finding
            IS DISTINCT FROM NEW.referenced_finding_id
       OR event_pass_result_referenced_run IS DISTINCT FROM (
           CASE
               WHEN NEW.referenced_finding_id IS NULL THEN NULL
               ELSE NEW.finding_run_id
           END
       )
       OR event_pass_result_referenced_pass IS DISTINCT FROM (
           SELECT producing_pass_id
             FROM review_finding
            WHERE finding_id = NEW.referenced_finding_id
              AND run_id = NEW.finding_run_id
              AND target_id = NEW.target_id
       )
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
           AND referenced.run_id = NEW.finding_run_id
           AND referenced.target_id = NEW.target_id;

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
            WITH RECURSIVE referenced_ancestry(finding_id) AS (
                SELECT NEW.referenced_finding_id
                UNION
                SELECT latest.referenced_finding_id
                  FROM referenced_ancestry AS ancestry
                  JOIN LATERAL (
                      SELECT referenced_finding_id
                        FROM review_finding_event
                       WHERE finding_id = ancestry.finding_id
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
