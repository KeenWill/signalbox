-- Reconcile provider events that committed before their exact GitHub write
-- receipt, independently of whether any active rule evaluates the event.

ALTER TABLE repo_watch_event
    ADD COLUMN snapshot_observed_at timestamptz;

CREATE FUNCTION reconcile_repo_watch_github_write_receipt()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    INSERT INTO repo_watch_event_self_cause (
        event_id, tool_attempt_id, cause_kind
    )
    SELECT event.event_id, NEW.tool_attempt_id,
           CASE NEW.operation_kind
               WHEN 'thread_reply' THEN 'thread_reply'
               WHEN 'thread_resolve' THEN 'thread_resolve'
               ELSE 'review_write'
           END
      FROM repo_watch_event AS event
     WHERE (
            event.event_kind = 'review_submitted'
            AND NEW.review_id = event.review_id
        ) OR (
            event.event_kind = 'thread_opened'
            AND NEW.operation_kind = 'publish_review'
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
                       = NEW.review_id
            )
        ) OR (
            event.event_kind = 'thread_opened'
            AND NEW.operation_kind = 'thread_reply'
            AND NEW.thread_id = event.thread_id
            AND NEW.tool_attempt_id < event.event_id
            AND event.recorded_at <= NEW.recorded_at
        ) OR (
            event.event_kind = 'thread_resolved'
            AND NEW.operation_kind = 'thread_resolve'
            AND NEW.thread_id = event.thread_id
            AND NEW.tool_attempt_id < event.event_id
            AND NEW.recorded_at <= event.snapshot_observed_at
        )
    ON CONFLICT (event_id) DO NOTHING;
    RETURN NULL;
END;
$$;

CREATE TRIGGER repo_watch_github_write_receipt_reconciles_existing_events
AFTER INSERT ON repo_watch_github_write_receipt
FOR EACH ROW
EXECUTE FUNCTION reconcile_repo_watch_github_write_receipt();
