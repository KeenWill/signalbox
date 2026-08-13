-- Exact GitHub object receipts for repository-watch self-cause suppression.

ALTER TABLE repo_watch_event
    ADD COLUMN review_id numeric(20, 0);

ALTER TABLE repo_watch_event
    ADD CONSTRAINT repo_watch_event_review_id_u64
        CHECK (review_id IS NULL OR review_id BETWEEN 1 AND 18446744073709551615),
    ADD CONSTRAINT repo_watch_event_review_id_shape
        CHECK ((review_id IS NOT NULL) = (event_kind = 'review_submitted'));

CREATE TABLE repo_watch_github_write_receipt (
    tool_attempt_id uuid PRIMARY KEY,
    session_id uuid NOT NULL,
    operation_kind text NOT NULL,
    repository text,
    pull_request_number numeric(20, 0),
    review_id numeric(20, 0),
    comment_id numeric(20, 0),
    comment_node_id text,
    thread_id text,
    recorded_at timestamptz NOT NULL DEFAULT transaction_timestamp(),

    FOREIGN KEY (tool_attempt_id)
        REFERENCES tool_attempt(attempt_id) ON UPDATE RESTRICT ON DELETE RESTRICT,
    FOREIGN KEY (tool_attempt_id, session_id)
        REFERENCES tool_attempt(attempt_id, session_id) ON UPDATE RESTRICT ON DELETE RESTRICT,
    CHECK (operation_kind IN ('publish_review', 'thread_reply', 'thread_resolve', 'comment')),
    CHECK (repository IS NULL OR repo_watch_repository_is_valid(repository)),
    CHECK (pull_request_number IS NULL OR pull_request_number BETWEEN 1 AND 18446744073709551615),
    CHECK (review_id IS NULL OR review_id BETWEEN 1 AND 18446744073709551615),
    CHECK (comment_id IS NULL OR comment_id BETWEEN 1 AND 18446744073709551615),
    CHECK (
        (operation_kind = 'publish_review' AND repository IS NOT NULL
            AND pull_request_number IS NOT NULL AND review_id IS NOT NULL
            AND comment_id IS NULL AND comment_node_id IS NULL AND thread_id IS NULL)
        OR
        (operation_kind = 'thread_reply' AND repository IS NULL
            AND pull_request_number IS NULL AND review_id IS NOT NULL
            AND comment_id IS NOT NULL AND comment_node_id IS NOT NULL AND thread_id IS NOT NULL)
        OR
        (operation_kind = 'thread_resolve' AND repository IS NULL
            AND pull_request_number IS NULL AND review_id IS NULL
            AND comment_id IS NULL AND comment_node_id IS NULL AND thread_id IS NOT NULL)
        OR
        (operation_kind = 'comment' AND repository IS NOT NULL
            AND pull_request_number IS NOT NULL AND review_id IS NULL
            AND comment_id IS NOT NULL AND comment_node_id IS NULL AND thread_id IS NULL)
    )
);

CREATE INDEX repo_watch_github_write_receipt_review
    ON repo_watch_github_write_receipt(review_id)
    WHERE review_id IS NOT NULL;

CREATE INDEX repo_watch_github_write_receipt_thread
    ON repo_watch_github_write_receipt(thread_id)
    WHERE thread_id IS NOT NULL;

CREATE FUNCTION record_repo_watch_github_write_receipt(candidate uuid)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    stored record;
    arguments jsonb;
    result jsonb;
BEGIN
    SELECT attempt.attempt_id, attempt.session_id, request.tool_name,
           request.arguments_text, attempt.result_text
      INTO stored
      FROM tool_attempt AS attempt
      JOIN tool_request AS request ON request.request_id = attempt.request_id
     WHERE attempt.attempt_id = candidate
       AND attempt.state_kind = 'terminal'
       AND attempt.terminal_disposition_kind = 'completed';

    IF NOT FOUND THEN
        RETURN;
    END IF;
    IF stored.tool_name NOT IN (
        'github_pull_request_publish_review',
        'change_request_thread_reply',
        'change_request_thread_resolve',
        'change_request_comment'
    ) THEN
        RETURN;
    END IF;
    arguments := stored.arguments_text::jsonb;
    result := stored.result_text::jsonb;

    IF stored.tool_name = 'github_pull_request_publish_review' THEN
        INSERT INTO repo_watch_github_write_receipt (
            tool_attempt_id, session_id, operation_kind, repository,
            pull_request_number, review_id
        ) VALUES (
            stored.attempt_id, stored.session_id, 'publish_review',
            arguments ->> 'repository', (arguments ->> 'number')::numeric,
            (result ->> 'id')::numeric
        ) ON CONFLICT (tool_attempt_id) DO NOTHING;
    ELSIF stored.tool_name = 'change_request_thread_reply' THEN
        INSERT INTO repo_watch_github_write_receipt (
            tool_attempt_id, session_id, operation_kind, review_id,
            comment_id, comment_node_id, thread_id
        ) VALUES (
            stored.attempt_id, stored.session_id, 'thread_reply',
            (result ->> 'review_id')::numeric, (result ->> 'comment_id')::numeric,
            result ->> 'comment_node_id', arguments ->> 'thread_id'
        ) ON CONFLICT (tool_attempt_id) DO NOTHING;
    ELSIF stored.tool_name = 'change_request_thread_resolve' THEN
        INSERT INTO repo_watch_github_write_receipt (
            tool_attempt_id, session_id, operation_kind, thread_id
        ) VALUES (
            stored.attempt_id, stored.session_id, 'thread_resolve',
            result ->> 'thread_id'
        ) ON CONFLICT (tool_attempt_id) DO NOTHING;
    ELSIF stored.tool_name = 'change_request_comment' THEN
        INSERT INTO repo_watch_github_write_receipt (
            tool_attempt_id, session_id, operation_kind, repository,
            pull_request_number, comment_id
        ) VALUES (
            stored.attempt_id, stored.session_id, 'comment',
            arguments ->> 'repository', (arguments ->> 'number')::numeric,
            (result ->> 'id')::numeric
        ) ON CONFLICT (tool_attempt_id) DO NOTHING;
    END IF;
END;
$$;

CREATE FUNCTION capture_repo_watch_github_write_receipt()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    PERFORM record_repo_watch_github_write_receipt(NEW.attempt_id);
    RETURN NULL;
END;
$$;

CREATE TRIGGER tool_attempt_capture_repo_watch_github_write_receipt
AFTER INSERT OR UPDATE OF state_kind, terminal_disposition_kind, result_text ON tool_attempt
FOR EACH ROW
WHEN (NEW.state_kind = 'terminal' AND NEW.terminal_disposition_kind = 'completed')
EXECUTE FUNCTION capture_repo_watch_github_write_receipt();

SELECT record_repo_watch_github_write_receipt(attempt.attempt_id)
  FROM tool_attempt AS attempt
  JOIN tool_request AS request ON request.request_id = attempt.request_id
 WHERE attempt.state_kind = 'terminal'
   AND attempt.terminal_disposition_kind = 'completed'
   AND (
       request.tool_name IN (
           'github_pull_request_publish_review',
           'change_request_thread_resolve',
           'change_request_comment'
       )
       OR CASE
           WHEN request.tool_name = 'change_request_thread_reply'
               THEN CASE
                   WHEN valid_tool_json(attempt.result_text)
                       THEN attempt.result_text::jsonb ?& ARRAY[
                           'comment_id', 'comment_node_id', 'review_id', 'url'
                       ]
                   ELSE false
               END
           ELSE false
       END
   );

CREATE TABLE repo_watch_github_write_observation (
    tool_attempt_id uuid PRIMARY KEY,
    repository text NOT NULL,
    cursor_generation bigint NOT NULL,
    FOREIGN KEY (tool_attempt_id)
        REFERENCES repo_watch_github_write_receipt(tool_attempt_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT,
    FOREIGN KEY (repository, cursor_generation)
        REFERENCES repo_watch_cursor(repository, generation)
        ON UPDATE RESTRICT ON DELETE RESTRICT
);

CREATE TABLE repo_watch_event_self_cause (
    event_id uuid PRIMARY KEY,
    tool_attempt_id uuid NOT NULL,
    cause_kind text NOT NULL,
    FOREIGN KEY (event_id)
        REFERENCES repo_watch_event(event_id) ON UPDATE RESTRICT ON DELETE RESTRICT,
    FOREIGN KEY (tool_attempt_id)
        REFERENCES repo_watch_github_write_receipt(tool_attempt_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT,
    CHECK (cause_kind IN ('review_write', 'thread_reply', 'thread_resolve'))
);

CREATE TRIGGER repo_watch_github_write_receipt_is_append_only
BEFORE UPDATE OR DELETE ON repo_watch_github_write_receipt
FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER repo_watch_github_write_observation_is_append_only
BEFORE UPDATE OR DELETE ON repo_watch_github_write_observation
FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER repo_watch_event_self_cause_is_append_only
BEFORE UPDATE OR DELETE ON repo_watch_event_self_cause
FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER repo_watch_github_write_receipt_reject_truncate
BEFORE TRUNCATE ON repo_watch_github_write_receipt
FOR EACH STATEMENT EXECUTE FUNCTION reject_repo_watch_table_truncate();

CREATE TRIGGER repo_watch_github_write_observation_reject_truncate
BEFORE TRUNCATE ON repo_watch_github_write_observation
FOR EACH STATEMENT EXECUTE FUNCTION reject_repo_watch_table_truncate();

CREATE TRIGGER repo_watch_event_self_cause_reject_truncate
BEFORE TRUNCATE ON repo_watch_event_self_cause
FOR EACH STATEMENT EXECUTE FUNCTION reject_repo_watch_table_truncate();

ALTER TABLE repo_watch_rule_evaluation
    DROP CONSTRAINT repo_watch_rule_evaluation_outcome_kind_check,
    DROP CONSTRAINT repo_watch_rule_evaluation_check,
    ADD CHECK (outcome_kind IN ('not_matched', 'self_caused', 'occupied', 'cooldown', 'dispatched')),
    ADD CHECK ((dispatch_id IS NOT NULL) = (outcome_kind = 'dispatched'));
