-- Append-only repository-watch cursors and closed version-one event facts.

CREATE TABLE repo_watch_cursor (
    repository text NOT NULL,
    generation bigint NOT NULL,
    storage_version smallint NOT NULL,
    cursor_payload jsonb NOT NULL,
    recorded_at timestamptz NOT NULL DEFAULT transaction_timestamp(),

    PRIMARY KEY (repository, generation),
    CHECK (octet_length(repository) BETWEEN 1 AND 201),
    CHECK (repository = lower(repository)),
    CHECK (generation > 0),
    CHECK (storage_version = 1),
    CHECK (jsonb_typeof(cursor_payload) = 'object'),
    CHECK ((cursor_payload ->> 'storage_version')::bigint = storage_version)
);

CREATE TABLE repo_watch_event (
    event_id uuid PRIMARY KEY,
    repository text NOT NULL,
    cursor_generation bigint NOT NULL,
    event_ordinal integer NOT NULL,
    event_version smallint NOT NULL,
    target_kind text NOT NULL,
    event_kind text NOT NULL,

    pull_request_number numeric(20, 0),
    head_sha text,
    head_repository text,
    base_branch text,
    head_branch text,
    title text,
    body text,
    labels text[],
    draft boolean,
    author text,

    previous_sha text,
    current_sha text,
    mergeable_state text,
    checks_outcome text,
    check_run_name text,
    conclusion text,
    workflow_branch text,
    workflow_name text,
    review_reviewer text,
    review_state text,
    review_commit text,
    thread_id text,
    label_name text,
    advanced_branch text,
    reaction_subject_kind text,
    reaction_subject_id numeric(20, 0),
    reaction_reactor text,
    reaction_content text,
    reaction_change text,

    recorded_at timestamptz NOT NULL DEFAULT transaction_timestamp(),

    UNIQUE (repository, cursor_generation, event_ordinal),
    FOREIGN KEY (repository, cursor_generation)
        REFERENCES repo_watch_cursor(repository, generation)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT,
    CHECK (event_ordinal > 0),
    CHECK (event_version = 1),
    CHECK (octet_length(repository) BETWEEN 1 AND 201),
    CHECK (repository = lower(repository)),
    CHECK (target_kind IN ('pull_request', 'branch')),
    CHECK (
        event_kind IN (
            'pull_request_opened',
            'pull_request_closed',
            'pull_request_merged',
            'head_changed',
            'mergeable_state_changed',
            'checks_completed',
            'check_run_completed',
            'branch_workflow_run_completed',
            'review_submitted',
            'thread_opened',
            'thread_resolved',
            'labeled',
            'unlabeled',
            'base_advanced',
            'reaction_changed'
        )
    ),
    CHECK (
        (
            target_kind = 'pull_request'
            AND event_kind <> 'branch_workflow_run_completed'
            AND pull_request_number > 0
            AND head_sha IS NOT NULL
            AND head_repository IS NOT NULL
            AND base_branch IS NOT NULL
            AND head_branch IS NOT NULL
            AND title IS NOT NULL
            AND body IS NOT NULL
            AND labels IS NOT NULL
            AND draft IS NOT NULL
        )
        OR (
            target_kind = 'branch'
            AND event_kind = 'branch_workflow_run_completed'
            AND pull_request_number IS NULL
            AND head_sha IS NULL
            AND head_repository IS NULL
            AND base_branch IS NULL
            AND head_branch IS NULL
            AND title IS NULL
            AND body IS NULL
            AND labels IS NULL
            AND draft IS NULL
            AND author IS NULL
        )
    ),
    CHECK (head_sha IS NULL OR head_sha ~ '^[0-9a-f]{40}$'),
    CHECK (current_sha IS NULL OR current_sha ~ '^[0-9a-f]{40}$'),
    CHECK (previous_sha IS NULL OR previous_sha ~ '^[0-9a-f]{40}$'),
    CHECK (review_commit IS NULL OR review_commit ~ '^[0-9a-f]{40}$'),
    CHECK (head_repository IS NULL OR head_repository = lower(head_repository)),
    CHECK (author IS NULL OR author = lower(author)),
    CHECK (review_reviewer IS NULL OR review_reviewer = lower(review_reviewer)),
    CHECK (reaction_reactor IS NULL OR reaction_reactor = lower(reaction_reactor)),
    CHECK (mergeable_state IS NULL OR mergeable_state IN ('mergeable', 'conflicting', 'unknown')),
    CHECK (checks_outcome IS NULL OR checks_outcome IN ('success', 'failure')),
    CHECK (
        conclusion IS NULL
        OR conclusion IN (
            'success', 'failure', 'neutral', 'cancelled', 'skipped',
            'timed_out', 'action_required', 'stale', 'startup_failure'
        )
    ),
    CHECK (review_state IS NULL OR review_state IN ('approved', 'changes_requested', 'commented')),
    CHECK (
        reaction_subject_kind IS NULL
        OR reaction_subject_kind IN ('pull_request_body', 'issue_comment', 'review_comment')
    ),
    CHECK (reaction_change IS NULL OR reaction_change IN ('added', 'removed')),
    CHECK (
        (reaction_subject_kind = 'pull_request_body' AND reaction_subject_id IS NULL)
        OR (
            reaction_subject_kind IN ('issue_comment', 'review_comment')
            AND reaction_subject_id > 0
        )
        OR (reaction_subject_kind IS NULL AND reaction_subject_id IS NULL)
    ),
    CHECK ((previous_sha IS NOT NULL) = (event_kind = 'head_changed')),
    CHECK ((current_sha IS NOT NULL) = (event_kind = 'head_changed')),
    CHECK ((mergeable_state IS NOT NULL) = (event_kind = 'mergeable_state_changed')),
    CHECK ((checks_outcome IS NOT NULL) = (event_kind = 'checks_completed')),
    CHECK ((check_run_name IS NOT NULL) = (event_kind = 'check_run_completed')),
    CHECK (
        (conclusion IS NOT NULL)
        = (event_kind IN ('check_run_completed', 'branch_workflow_run_completed'))
    ),
    CHECK ((workflow_branch IS NOT NULL) = (event_kind = 'branch_workflow_run_completed')),
    CHECK ((workflow_name IS NOT NULL) = (event_kind = 'branch_workflow_run_completed')),
    CHECK ((review_reviewer IS NOT NULL) = (event_kind = 'review_submitted')),
    CHECK ((review_state IS NOT NULL) = (event_kind = 'review_submitted')),
    CHECK ((review_commit IS NOT NULL) = (event_kind = 'review_submitted')),
    CHECK ((thread_id IS NOT NULL) = (event_kind IN ('thread_opened', 'thread_resolved'))),
    CHECK ((label_name IS NOT NULL) = (event_kind IN ('labeled', 'unlabeled'))),
    CHECK ((advanced_branch IS NOT NULL) = (event_kind = 'base_advanced')),
    CHECK ((reaction_subject_kind IS NOT NULL) = (event_kind = 'reaction_changed')),
    CHECK ((reaction_reactor IS NOT NULL) = (event_kind = 'reaction_changed')),
    CHECK ((reaction_content IS NOT NULL) = (event_kind = 'reaction_changed')),
    CHECK ((reaction_change IS NOT NULL) = (event_kind = 'reaction_changed')),
    CHECK (previous_sha IS NULL OR previous_sha <> current_sha),
    CHECK (current_sha IS NULL OR current_sha = head_sha),
    CHECK (advanced_branch IS NULL OR advanced_branch = base_branch),
    CHECK (event_kind <> 'labeled' OR label_name = ANY(labels)),
    CHECK (event_kind <> 'unlabeled' OR NOT (label_name = ANY(labels))),
    CHECK (head_repository IS NULL OR octet_length(head_repository) BETWEEN 1 AND 201),
    CHECK (base_branch IS NULL OR octet_length(base_branch) BETWEEN 1 AND 255),
    CHECK (head_branch IS NULL OR octet_length(head_branch) BETWEEN 1 AND 255),
    CHECK (title IS NULL OR octet_length(title) BETWEEN 1 AND 1024),
    CHECK (body IS NULL OR octet_length(body) <= 262144),
    CHECK (label_name IS NULL OR octet_length(label_name) BETWEEN 1 AND 200),
    CHECK (thread_id IS NULL OR octet_length(thread_id) BETWEEN 1 AND 256),
    CHECK (check_run_name IS NULL OR octet_length(check_run_name) BETWEEN 1 AND 256),
    CHECK (workflow_name IS NULL OR octet_length(workflow_name) BETWEEN 1 AND 256),
    CHECK (reaction_content IS NULL OR octet_length(reaction_content) BETWEEN 1 AND 64)
);

CREATE FUNCTION reject_repo_watch_table_truncate()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION '% cannot be truncated', TG_TABLE_NAME
        USING ERRCODE = '23514';
END;
$$;

CREATE TRIGGER repo_watch_cursor_is_append_only
BEFORE UPDATE OR DELETE ON repo_watch_cursor
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER repo_watch_event_is_append_only
BEFORE UPDATE OR DELETE ON repo_watch_event
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER repo_watch_cursor_reject_truncate
BEFORE TRUNCATE ON repo_watch_cursor
FOR EACH STATEMENT
EXECUTE FUNCTION reject_repo_watch_table_truncate();

CREATE TRIGGER repo_watch_event_reject_truncate
BEFORE TRUNCATE ON repo_watch_event
FOR EACH STATEMENT
EXECUTE FUNCTION reject_repo_watch_table_truncate();
