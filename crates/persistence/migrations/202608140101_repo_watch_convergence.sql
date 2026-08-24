-- Record exact-head pull-request convergence, stop its commission, and seal
-- that head against later repository-watch dispatch.

-- Supersedes the outcome vocabulary from
-- 202608140100_repo_watch_dispatch_release.sql.
ALTER TABLE repo_watch_rule_evaluation
    DROP CONSTRAINT repo_watch_rule_evaluation_outcome_kind_check;

ALTER TABLE repo_watch_rule_evaluation
    ADD CONSTRAINT repo_watch_rule_evaluation_outcome_kind_check
    CHECK (outcome_kind IN (
        'not_matched', 'target_closed', 'target_converged', 'occupied',
        'coalesced', 'cooldown', 'dispatched'
    ));

-- Supersedes the settlement vocabulary and shape from
-- 202608140100_repo_watch_dispatch_release.sql.
ALTER TABLE repo_watch_dispatch_obligation
    DROP CONSTRAINT repo_watch_dispatch_obligation_settled_kind_check,
    DROP CONSTRAINT repo_watch_dispatch_obligation_settlement_shape_check;

ALTER TABLE repo_watch_dispatch_obligation
    ADD CONSTRAINT repo_watch_dispatch_obligation_settled_kind_check
    CHECK (
        settled_kind IS NULL
        OR settled_kind IN (
            'dispatched', 'deactivated', 'target_closed', 'target_converged'
        )
    ),
    ADD CONSTRAINT repo_watch_dispatch_obligation_settlement_shape_check
    CHECK (
        (settled_kind IS NULL
            AND settled_dispatch_id IS NULL
            AND settled_at IS NULL)
        OR (settled_kind = 'dispatched'
            AND settled_dispatch_id IS NOT NULL
            AND settled_at IS NOT NULL)
        OR (settled_kind IN (
                'deactivated', 'target_closed', 'target_converged'
            )
            AND settled_dispatch_id IS NULL
            AND settled_at IS NOT NULL)
    );

CREATE FUNCTION repo_watch_convergence_threads_are_valid(candidate text[])
RETURNS boolean
LANGUAGE sql
IMMUTABLE
STRICT
PARALLEL SAFE
SET search_path TO public, pg_catalog, pg_temp
AS $$
    SELECT COALESCE(array_ndims(candidate), 1) = 1
       AND COALESCE(array_lower(candidate, 1), 1) = 1
       AND cardinality(candidate) <= 10000
       AND candidate = ARRAY(
            SELECT DISTINCT value COLLATE "C"
              FROM unnest(candidate) AS item(value)
             ORDER BY value COLLATE "C"
       )
       AND NOT EXISTS (
            SELECT 1 FROM unnest(candidate) AS item(value)
             WHERE value IS NULL OR octet_length(value) NOT BETWEEN 1 AND 256
       )
$$;

CREATE FUNCTION repo_watch_convergence_check_names_are_valid(candidate text[])
RETURNS boolean
LANGUAGE sql
IMMUTABLE
STRICT
PARALLEL SAFE
SET search_path TO public, pg_catalog, pg_temp
AS $$
    SELECT COALESCE(array_ndims(candidate), 1) = 1
       AND COALESCE(array_lower(candidate, 1), 1) = 1
       AND cardinality(candidate) <= 10000
       AND candidate = ARRAY(
            SELECT value COLLATE "C"
              FROM unnest(candidate) AS item(value)
             ORDER BY value COLLATE "C"
       )
       AND NOT EXISTS (
            SELECT 1 FROM unnest(candidate) AS item(value)
             WHERE value IS NULL OR octet_length(value) NOT BETWEEN 1 AND 256
       )
$$;

CREATE TABLE repo_watch_pull_request_convergence_assessment (
    assessment_id uuid PRIMARY KEY,
    repository text NOT NULL,
    cursor_generation bigint NOT NULL,
    pull_request_number numeric(20, 0) NOT NULL,
    head_sha text NOT NULL,
    base_branch text NOT NULL,
    base_revision text NOT NULL,
    mergeable_state text NOT NULL,
    settled boolean NOT NULL,
    review_decision text NOT NULL,
    unresolved_threads text[] NOT NULL,
    gating_check_count bigint NOT NULL,
    non_green_gating_checks text[] NOT NULL,
    verdict_kind text NOT NULL,
    recorded_at timestamptz NOT NULL DEFAULT clock_timestamp(),

    FOREIGN KEY (repository, cursor_generation)
        REFERENCES repo_watch_cursor(repository, generation)
        ON UPDATE RESTRICT ON DELETE RESTRICT,
    CHECK (repo_watch_repository_is_valid(repository)),
    CHECK (
        pull_request_number > 0
        AND pull_request_number <= 18446744073709551615
    ),
    CHECK (head_sha COLLATE "C" ~ '^[0-9a-f]{40}$'),
    CHECK (repo_watch_branch_is_valid(base_branch)),
    CHECK (base_revision COLLATE "C" ~ '^[0-9a-f]{40}$'),
    CHECK (mergeable_state IN ('mergeable', 'conflicting', 'unknown')),
    CHECK (review_decision IN (
        'none', 'approved', 'review_required', 'changes_requested'
    )),
    CHECK (repo_watch_convergence_threads_are_valid(unresolved_threads)),
    CHECK (gating_check_count >= 0 AND gating_check_count <= 10000),
    CHECK (cardinality(non_green_gating_checks) <= gating_check_count),
    CHECK (repo_watch_convergence_check_names_are_valid(non_green_gating_checks)),
    CHECK (verdict_kind IN (
        'not_converged', 'internally_converged', 'merge_ready'
    )),
    CHECK (verdict_kind <> 'merge_ready' OR base_branch = 'main'),
    CHECK (verdict_kind <> 'internally_converged' OR base_branch <> 'main'),
    CONSTRAINT repo_watch_convergence_verdict_matches_evidence CHECK (
        (verdict_kind = 'not_converged')
        = (
            cardinality(unresolved_threads) > 0
            OR cardinality(non_green_gating_checks) > 0
            OR mergeable_state <> 'mergeable'
            OR NOT settled
            OR gating_check_count = 0
            OR review_decision = 'changes_requested'
        )
    ),
    UNIQUE (
        assessment_id, repository, pull_request_number, head_sha,
        base_revision, verdict_kind
    ),
    UNIQUE (
        assessment_id, repository, pull_request_number
    )
);

CREATE TABLE repo_watch_pull_request_convergence (
    repository text NOT NULL,
    pull_request_number numeric(20, 0) NOT NULL,
    head_sha text NOT NULL,
    base_revision text NOT NULL,
    assessment_id uuid NOT NULL UNIQUE,
    convergence_kind text NOT NULL CHECK (
        convergence_kind IN ('internally_converged', 'merge_ready')
    ),
    converged_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (repository, pull_request_number, head_sha, base_revision),
    CHECK (base_revision COLLATE "C" ~ '^[0-9a-f]{40}$'),
    CONSTRAINT repo_watch_convergence_assessment_matches
    FOREIGN KEY (
        assessment_id, repository, pull_request_number, head_sha,
        base_revision, convergence_kind
    ) REFERENCES repo_watch_pull_request_convergence_assessment(
        assessment_id, repository, pull_request_number, head_sha,
        base_revision, verdict_kind
    )
        ON UPDATE RESTRICT ON DELETE RESTRICT
);

CREATE INDEX repo_watch_convergence_assessment_current_idx
    ON repo_watch_pull_request_convergence_assessment (
        repository, pull_request_number, recorded_at DESC, assessment_id DESC
    );

CREATE TABLE repo_watch_pull_request_convergence_identity (
    identity_id uuid PRIMARY KEY,
    repository text NOT NULL,
    cursor_generation bigint NOT NULL,
    pull_request_number numeric(20, 0) NOT NULL,
    assessment_id uuid NOT NULL,
    recorded_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    UNIQUE (identity_id, assessment_id, cursor_generation),
    FOREIGN KEY (repository, cursor_generation)
        REFERENCES repo_watch_cursor(repository, generation)
        ON UPDATE RESTRICT ON DELETE RESTRICT,
    FOREIGN KEY (assessment_id, repository, pull_request_number)
        REFERENCES repo_watch_pull_request_convergence_assessment(
            assessment_id, repository, pull_request_number
        )
        ON UPDATE RESTRICT ON DELETE RESTRICT
);

CREATE INDEX repo_watch_convergence_identity_current_idx
    ON repo_watch_pull_request_convergence_identity (
        repository, pull_request_number, cursor_generation DESC,
        recorded_at DESC, identity_id DESC
    );

CREATE TABLE repo_watch_convergence_cutoff (
    assessment_id uuid NOT NULL,
    identity_id uuid NOT NULL,
    identity_assessment_id uuid NOT NULL,
    cursor_generation bigint NOT NULL,
    processed_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    PRIMARY KEY (assessment_id, identity_id),
    FOREIGN KEY (assessment_id)
        REFERENCES repo_watch_pull_request_convergence(assessment_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT,
    FOREIGN KEY (identity_id, identity_assessment_id, cursor_generation)
        REFERENCES repo_watch_pull_request_convergence_identity(
            identity_id, assessment_id, cursor_generation
        )
        ON UPDATE RESTRICT ON DELETE RESTRICT
);

CREATE TABLE repo_watch_convergence_cutoff_goal (
    assessment_id uuid NOT NULL,
    identity_id uuid NOT NULL,
    session_id uuid NOT NULL,
    goal_command_id uuid NOT NULL,
    PRIMARY KEY (assessment_id, identity_id, session_id),
    UNIQUE (goal_command_id),
    FOREIGN KEY (assessment_id, identity_id)
        REFERENCES repo_watch_convergence_cutoff(
            assessment_id, identity_id
        )
        ON UPDATE RESTRICT ON DELETE RESTRICT,
    FOREIGN KEY (goal_command_id, session_id)
        REFERENCES goal_command(command_id, session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT
);

CREATE VIEW repo_watch_current_pull_request_convergence AS
SELECT DISTINCT ON (identity.repository, identity.pull_request_number)
       assessment.repository,
       assessment.pull_request_number,
       assessment.head_sha,
       assessment.base_branch,
       assessment.base_revision,
       assessment.mergeable_state,
       assessment.settled,
       assessment.review_decision,
       cardinality(assessment.unresolved_threads) AS unresolved_thread_count,
       assessment.gating_check_count,
       assessment.non_green_gating_checks,
       assessment.verdict_kind,
       convergence.convergence_kind AS sealed_kind,
       convergence.converged_at,
       assessment.recorded_at
  FROM repo_watch_pull_request_convergence_identity AS identity
  JOIN repo_watch_pull_request_convergence_assessment AS assessment
    ON assessment.assessment_id = identity.assessment_id
  LEFT JOIN repo_watch_pull_request_convergence AS convergence
    ON convergence.repository = assessment.repository
   AND convergence.pull_request_number = assessment.pull_request_number
   AND convergence.head_sha = assessment.head_sha
   AND convergence.base_revision = assessment.base_revision
 ORDER BY identity.repository, identity.pull_request_number,
          identity.cursor_generation DESC, identity.recorded_at DESC,
          identity.identity_id DESC;

CREATE TRIGGER repo_watch_convergence_assessment_is_append_only
BEFORE UPDATE OR DELETE ON repo_watch_pull_request_convergence_assessment
FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER repo_watch_convergence_identity_is_append_only
BEFORE UPDATE OR DELETE ON repo_watch_pull_request_convergence_identity
FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER repo_watch_pull_request_convergence_is_append_only
BEFORE UPDATE OR DELETE ON repo_watch_pull_request_convergence
FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER repo_watch_convergence_cutoff_is_append_only
BEFORE UPDATE OR DELETE ON repo_watch_convergence_cutoff
FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER repo_watch_convergence_cutoff_goal_is_append_only
BEFORE UPDATE OR DELETE ON repo_watch_convergence_cutoff_goal
FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER repo_watch_convergence_assessment_reject_truncate
BEFORE TRUNCATE ON repo_watch_pull_request_convergence_assessment
FOR EACH STATEMENT EXECUTE FUNCTION reject_repo_watch_table_truncate();

CREATE TRIGGER repo_watch_convergence_identity_reject_truncate
BEFORE TRUNCATE ON repo_watch_pull_request_convergence_identity
FOR EACH STATEMENT EXECUTE FUNCTION reject_repo_watch_table_truncate();

CREATE TRIGGER repo_watch_pull_request_convergence_reject_truncate
BEFORE TRUNCATE ON repo_watch_pull_request_convergence
FOR EACH STATEMENT EXECUTE FUNCTION reject_repo_watch_table_truncate();

CREATE TRIGGER repo_watch_convergence_cutoff_reject_truncate
BEFORE TRUNCATE ON repo_watch_convergence_cutoff
FOR EACH STATEMENT EXECUTE FUNCTION reject_repo_watch_table_truncate();

CREATE TRIGGER repo_watch_convergence_cutoff_goal_reject_truncate
BEFORE TRUNCATE ON repo_watch_convergence_cutoff_goal
FOR EACH STATEMENT EXECUTE FUNCTION reject_repo_watch_table_truncate();
