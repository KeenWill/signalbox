-- Plan and audit conservative dismissal of stale blocking pull-request reviews.

CREATE TABLE repo_watch_stale_review_clearance (
    clearance_id uuid PRIMARY KEY,
    assessment_id uuid NOT NULL,
    repository text NOT NULL,
    pull_request_number numeric(20, 0) NOT NULL,
    current_head_sha text NOT NULL,
    base_revision text NOT NULL,
    review_node_id text NOT NULL,
    reviewer text NOT NULL,
    reviewed_head_sha text NOT NULL,
    assessment_verdict text NOT NULL DEFAULT 'not_converged',
    reason_kind text NOT NULL DEFAULT 'only_stale_review_blocks',
    dismissal_message text NOT NULL,
    planned_at timestamptz NOT NULL DEFAULT clock_timestamp(),

    UNIQUE (assessment_id, review_node_id),
    CONSTRAINT repo_watch_stale_review_clearance_assessment_matches
    FOREIGN KEY (
        assessment_id, repository, pull_request_number, current_head_sha,
        base_revision, assessment_verdict
    ) REFERENCES repo_watch_pull_request_convergence_assessment(
        assessment_id, repository, pull_request_number, head_sha,
        base_revision, verdict_kind
    )
        ON UPDATE RESTRICT ON DELETE RESTRICT,
    CHECK (repo_watch_repository_is_valid(repository)),
    CHECK (
        pull_request_number > 0
        AND pull_request_number <= 18446744073709551615
    ),
    CHECK (current_head_sha COLLATE "C" ~ '^[0-9a-f]{40}$'),
    CHECK (base_revision COLLATE "C" ~ '^[0-9a-f]{40}$'),
    CHECK (reviewed_head_sha COLLATE "C" ~ '^[0-9a-f]{40}$'),
    CHECK (reviewed_head_sha <> current_head_sha),
    CHECK (octet_length(review_node_id) BETWEEN 1 AND 256),
    CHECK (repo_watch_login_is_valid(reviewer)),
    CHECK (assessment_verdict = 'not_converged'),
    CHECK (reason_kind = 'only_stale_review_blocks'),
    CHECK (octet_length(dismissal_message) BETWEEN 1 AND 1024)
);

CREATE TABLE repo_watch_stale_review_clearance_result (
    clearance_id uuid PRIMARY KEY,
    outcome_kind text NOT NULL,
    provider_review_state text NOT NULL,
    observed_at timestamptz NOT NULL DEFAULT clock_timestamp(),

    FOREIGN KEY (clearance_id)
        REFERENCES repo_watch_stale_review_clearance(clearance_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT,
    CHECK (outcome_kind IN (
        'dismissed', 'already_dismissed', 'cleared_elsewhere', 'superseded'
    )),
    CHECK (provider_review_state IN (
        'approved', 'changes_requested', 'commented', 'dismissed', 'pending'
    )),
    CHECK (
        (outcome_kind IN ('dismissed', 'already_dismissed')
            AND provider_review_state = 'dismissed')
        OR (outcome_kind = 'cleared_elsewhere'
            AND provider_review_state IN (
                'approved', 'changes_requested', 'commented', 'pending'
            ))
        OR (outcome_kind = 'superseded')
    )
);

-- A mutable, expiring delivery claim is separate from the append-only intent
-- and result journals. It prevents concurrent watchers from issuing the same
-- provider mutation while allowing recovery after a crashed claimant.
CREATE TABLE repo_watch_stale_review_clearance_claim (
    clearance_id uuid PRIMARY KEY,
    claim_token uuid NOT NULL UNIQUE,
    claimed_until timestamptz NOT NULL,

    FOREIGN KEY (clearance_id)
        REFERENCES repo_watch_stale_review_clearance(clearance_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT
);

CREATE INDEX repo_watch_stale_review_clearance_recovery_idx
    ON repo_watch_stale_review_clearance (repository, clearance_id);

-- Recovery advances durably between bounded scans. A null cursor starts at the
-- oldest pending intent; reaching the end wraps the next scan to the start.
CREATE TABLE repo_watch_stale_review_clearance_recovery_cursor (
    repository text PRIMARY KEY,
    after_clearance_id uuid,
    CHECK (repo_watch_repository_is_valid(repository))
);

CREATE VIEW repo_watch_pending_stale_review_clearance AS
SELECT clearance.clearance_id,
       clearance.assessment_id,
       clearance.repository,
       clearance.pull_request_number,
       clearance.current_head_sha,
       clearance.review_node_id,
       clearance.reviewer,
       clearance.reviewed_head_sha,
       clearance.reason_kind,
       clearance.dismissal_message,
       clearance.planned_at
  FROM repo_watch_stale_review_clearance AS clearance
  LEFT JOIN repo_watch_stale_review_clearance_result AS result
    ON result.clearance_id = clearance.clearance_id
 WHERE result.clearance_id IS NULL;

CREATE TRIGGER repo_watch_stale_review_clearance_is_append_only
BEFORE UPDATE OR DELETE ON repo_watch_stale_review_clearance
FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER repo_watch_stale_review_clearance_result_is_append_only
BEFORE UPDATE OR DELETE ON repo_watch_stale_review_clearance_result
FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER repo_watch_stale_review_clearance_reject_truncate
BEFORE TRUNCATE ON repo_watch_stale_review_clearance
FOR EACH STATEMENT EXECUTE FUNCTION reject_repo_watch_table_truncate();

CREATE TRIGGER repo_watch_stale_review_clearance_result_reject_truncate
BEFORE TRUNCATE ON repo_watch_stale_review_clearance_result
FOR EACH STATEMENT EXECUTE FUNCTION reject_repo_watch_table_truncate();

CREATE TRIGGER repo_watch_stale_review_clearance_claim_reject_truncate
BEFORE TRUNCATE ON repo_watch_stale_review_clearance_claim
FOR EACH STATEMENT EXECUTE FUNCTION reject_repo_watch_table_truncate();

CREATE TRIGGER repo_watch_stale_review_clearance_recovery_cursor_reject_truncate
BEFORE TRUNCATE ON repo_watch_stale_review_clearance_recovery_cursor
FOR EACH STATEMENT EXECUTE FUNCTION reject_repo_watch_table_truncate();
