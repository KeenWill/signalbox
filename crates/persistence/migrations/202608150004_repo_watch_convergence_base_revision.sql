-- Carry live convergence evidence across the base-revision identity correction.

DROP VIEW repo_watch_current_pull_request_convergence;

ALTER TABLE repo_watch_stale_review_clearance
    DROP CONSTRAINT repo_watch_stale_review_clearance_assessment_matches;

ALTER TABLE repo_watch_pull_request_convergence
    DROP CONSTRAINT repo_watch_convergence_assessment_matches,
    DROP CONSTRAINT repo_watch_pull_request_convergence_pkey;

ALTER TABLE repo_watch_pull_request_convergence_assessment
    DROP CONSTRAINT repo_watch_pull_request_conve_assessment_id_repository_pull_key;

ALTER TABLE repo_watch_pull_request_convergence_assessment
    ADD COLUMN base_revision text;

ALTER TABLE repo_watch_pull_request_convergence
    ADD COLUMN base_revision text;

ALTER TABLE repo_watch_stale_review_clearance
    ADD COLUMN base_revision text;

ALTER TABLE repo_watch_pull_request_convergence_assessment
    DISABLE TRIGGER repo_watch_convergence_assessment_is_append_only;

UPDATE repo_watch_pull_request_convergence_assessment AS assessment
   SET base_revision = (
       SELECT branch_head ->> 'head'
         FROM repo_watch_cursor AS cursor
        CROSS JOIN LATERAL jsonb_array_elements(
            cursor.cursor_payload -> 'state' -> 'branch_heads'
        ) AS branch_head
        WHERE cursor.repository = assessment.repository
          AND cursor.generation = assessment.cursor_generation
          AND branch_head ->> 'branch' = assessment.base_branch
   );

ALTER TABLE repo_watch_pull_request_convergence_assessment
    ENABLE TRIGGER repo_watch_convergence_assessment_is_append_only;

DO $$
DECLARE
    unresolved_assessment_id uuid;
BEGIN
    SELECT assessment_id
      INTO unresolved_assessment_id
      FROM repo_watch_pull_request_convergence_assessment
     WHERE base_revision IS NULL
        OR base_revision COLLATE "C" !~ '^[0-9a-f]{40}$'
     ORDER BY assessment_id
     LIMIT 1;
    IF unresolved_assessment_id IS NOT NULL THEN
        RAISE EXCEPTION
            'repository-watch convergence assessment % has no resolvable base revision',
            unresolved_assessment_id;
    END IF;
END
$$;

ALTER TABLE repo_watch_pull_request_convergence
    DISABLE TRIGGER repo_watch_pull_request_convergence_is_append_only;

UPDATE repo_watch_pull_request_convergence AS convergence
   SET base_revision = assessment.base_revision
  FROM repo_watch_pull_request_convergence_assessment AS assessment
 WHERE assessment.assessment_id = convergence.assessment_id;

ALTER TABLE repo_watch_pull_request_convergence
    ENABLE TRIGGER repo_watch_pull_request_convergence_is_append_only;

ALTER TABLE repo_watch_stale_review_clearance
    DISABLE TRIGGER repo_watch_stale_review_clearance_is_append_only;

UPDATE repo_watch_stale_review_clearance AS clearance
   SET base_revision = assessment.base_revision
  FROM repo_watch_pull_request_convergence_assessment AS assessment
 WHERE assessment.assessment_id = clearance.assessment_id;

ALTER TABLE repo_watch_stale_review_clearance
    ENABLE TRIGGER repo_watch_stale_review_clearance_is_append_only;

ALTER TABLE repo_watch_pull_request_convergence_assessment
    ALTER COLUMN base_revision SET NOT NULL,
    ADD CONSTRAINT repo_watch_convergence_assessment_base_revision_check
        CHECK (base_revision COLLATE "C" ~ '^[0-9a-f]{40}$'),
    ADD CONSTRAINT repo_watch_convergence_assessment_identity_unique
        UNIQUE (
            assessment_id, repository, pull_request_number, head_sha,
            base_revision, verdict_kind
        );

ALTER TABLE repo_watch_pull_request_convergence
    ALTER COLUMN base_revision SET NOT NULL,
    ADD CONSTRAINT repo_watch_convergence_base_revision_check
        CHECK (base_revision COLLATE "C" ~ '^[0-9a-f]{40}$'),
    ADD CONSTRAINT repo_watch_pull_request_convergence_pkey PRIMARY KEY (
        repository, pull_request_number, head_sha, base_revision
    ),
    ADD CONSTRAINT repo_watch_convergence_assessment_matches
        FOREIGN KEY (
            assessment_id, repository, pull_request_number, head_sha,
            base_revision, convergence_kind
        ) REFERENCES repo_watch_pull_request_convergence_assessment(
            assessment_id, repository, pull_request_number, head_sha,
            base_revision, verdict_kind
        ) ON UPDATE RESTRICT ON DELETE RESTRICT;

ALTER TABLE repo_watch_stale_review_clearance
    ALTER COLUMN base_revision SET NOT NULL,
    ADD CONSTRAINT repo_watch_stale_review_clearance_base_revision_check
        CHECK (base_revision COLLATE "C" ~ '^[0-9a-f]{40}$'),
    ADD CONSTRAINT repo_watch_stale_review_clearance_assessment_matches
        FOREIGN KEY (
            assessment_id, repository, pull_request_number, current_head_sha,
            base_revision, assessment_verdict
        ) REFERENCES repo_watch_pull_request_convergence_assessment(
            assessment_id, repository, pull_request_number, head_sha,
            base_revision, verdict_kind
        ) ON UPDATE RESTRICT ON DELETE RESTRICT;

CREATE VIEW repo_watch_current_pull_request_convergence AS
SELECT DISTINCT ON (assessment.repository, assessment.pull_request_number)
       assessment.repository,
       assessment.pull_request_number,
       assessment.head_sha,
       assessment.base_branch,
       assessment.base_revision,
       assessment.mergeable_state,
       assessment.review_decision,
       cardinality(assessment.unresolved_threads) AS unresolved_thread_count,
       assessment.gating_check_count,
       assessment.non_green_gating_checks,
       assessment.verdict_kind,
       convergence.convergence_kind AS sealed_kind,
       convergence.converged_at,
       assessment.recorded_at
  FROM repo_watch_pull_request_convergence_assessment AS assessment
  LEFT JOIN repo_watch_pull_request_convergence AS convergence
    ON convergence.repository = assessment.repository
   AND convergence.pull_request_number = assessment.pull_request_number
   AND convergence.head_sha = assessment.head_sha
   AND convergence.base_revision = assessment.base_revision
 ORDER BY assessment.repository, assessment.pull_request_number,
          assessment.recorded_at DESC, assessment.assessment_id DESC;
