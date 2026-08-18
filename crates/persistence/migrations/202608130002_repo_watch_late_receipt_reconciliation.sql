-- Reconcile provider events that committed before their exact GitHub write
-- receipt, independently of whether any active rule evaluates the event.

ALTER TABLE repo_watch_event
    ADD COLUMN snapshot_observed_at timestamptz;

CREATE INDEX repo_watch_event_review_submitted_review_id
    ON repo_watch_event(review_id)
    WHERE event_kind = 'review_submitted';

CREATE INDEX repo_watch_event_thread_opened_thread_id
    ON repo_watch_event(thread_id)
    WHERE event_kind = 'thread_opened';

CREATE INDEX repo_watch_event_thread_resolved_thread_id
    ON repo_watch_event(thread_id)
    WHERE event_kind = 'thread_resolved';

CREATE FUNCTION reconcile_repo_watch_github_write_receipt(
    candidate repo_watch_github_write_receipt
)
RETURNS void
LANGUAGE plpgsql
AS $$
BEGIN
    INSERT INTO repo_watch_event_self_cause (
        event_id, tool_attempt_id, cause_kind
    )
    SELECT event.event_id, candidate.tool_attempt_id,
           CASE candidate.operation_kind
               WHEN 'thread_reply' THEN 'thread_reply'
               WHEN 'thread_resolve' THEN 'thread_resolve'
               ELSE 'review_write'
           END
      FROM repo_watch_event AS event
     WHERE (
            event.event_kind = 'review_submitted'
            AND candidate.review_id = event.review_id
        ) OR (
            event.event_kind = 'thread_opened'
            AND candidate.operation_kind = 'publish_review'
            AND EXISTS (
                SELECT 1
                  FROM repo_watch_cursor AS cursor_record
                  CROSS JOIN LATERAL jsonb_array_elements(
                      cursor_record.cursor_payload -> 'state' -> 'pull_requests'
                  ) AS pull_request(value)
                  CROSS JOIN LATERAL jsonb_array_elements(
                      pull_request.value -> 'threads'
                  ) AS thread(value)
                 WHERE cursor_record.repository = event.repository
                   AND cursor_record.generation = event.cursor_generation
                   AND thread.value ->> 'thread' = event.thread_id
                   AND (thread.value ->> 'originating_review_id')::numeric
                       = candidate.review_id
            )
        ) OR (
            event.event_kind = 'thread_opened'
            AND candidate.operation_kind = 'thread_reply'
            AND candidate.thread_id = event.thread_id
            AND candidate.tool_attempt_id < event.event_id
            AND event.recorded_at <= candidate.recorded_at
        ) OR (
            event.event_kind = 'thread_resolved'
            AND candidate.operation_kind = 'thread_resolve'
            AND candidate.thread_id = event.thread_id
            AND candidate.tool_attempt_id < event.event_id
            AND candidate.recorded_at <= event.snapshot_observed_at
            AND EXISTS (
                SELECT 1
                  FROM repo_watch_github_write_observation AS observed
                 WHERE observed.tool_attempt_id = candidate.tool_attempt_id
                   AND observed.repository = event.repository
                   AND observed.cursor_generation = event.cursor_generation
            )
        )
    ON CONFLICT (event_id) DO NOTHING;
END;
$$;

CREATE FUNCTION trigger_reconcile_repo_watch_github_write_receipt()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    PERFORM reconcile_repo_watch_github_write_receipt(NEW);
    RETURN NULL;
END;
$$;

CREATE TRIGGER repo_watch_github_write_receipt_reconciles_existing_events
AFTER INSERT ON repo_watch_github_write_receipt
FOR EACH ROW
EXECUTE FUNCTION trigger_reconcile_repo_watch_github_write_receipt();

SELECT reconcile_repo_watch_github_write_receipt(receipt)
  FROM repo_watch_github_write_receipt AS receipt;
