-- Bound the remaining repository-watch operator aggregations and align visible
-- obligation readiness with the production redispatch policy.

ALTER TABLE repo_watch_webhook_projection
    ADD COLUMN repository text,
    ADD COLUMN received_at timestamptz;

DROP TRIGGER repo_watch_webhook_projection_is_append_only
    ON repo_watch_webhook_projection;

UPDATE repo_watch_webhook_projection AS projection
   SET repository = delivery.repository,
       received_at = delivery.received_at
  FROM repo_watch_webhook_delivery AS delivery
 WHERE delivery.hook_id = projection.hook_id
   AND delivery.delivery_id = projection.delivery_id;

ALTER TABLE repo_watch_webhook_projection
    ALTER COLUMN repository SET NOT NULL,
    ALTER COLUMN received_at SET NOT NULL,
    ADD CONSTRAINT repo_watch_webhook_projection_repository_check
        CHECK (repo_watch_repository_is_valid(repository));

CREATE FUNCTION stamp_repo_watch_webhook_projection_delivery()
RETURNS trigger
LANGUAGE plpgsql
SET search_path FROM CURRENT
AS $$
BEGIN
    SELECT delivery.repository, delivery.received_at
      INTO NEW.repository, NEW.received_at
      FROM repo_watch_webhook_delivery AS delivery
     WHERE delivery.hook_id = NEW.hook_id
       AND delivery.delivery_id = NEW.delivery_id;
    RETURN NEW;
END;
$$;

CREATE TRIGGER repo_watch_webhook_projection_stamps_delivery
BEFORE INSERT ON repo_watch_webhook_projection
FOR EACH ROW EXECUTE FUNCTION stamp_repo_watch_webhook_projection_delivery();

CREATE TRIGGER repo_watch_webhook_projection_is_append_only
BEFORE UPDATE OR DELETE ON repo_watch_webhook_projection
FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE INDEX repo_watch_webhook_projection_repository_time
    ON repo_watch_webhook_projection (
        repository, projected_at DESC, delivery_id, projection_ordinal DESC
    );

CREATE INDEX repo_watch_dispatch_obligation_latest_event
    ON repo_watch_dispatch_obligation (latest_event_id)
    WHERE settled_kind IS NULL;

CREATE TABLE repo_watch_current_pull_request_session_count (
    repository text NOT NULL,
    pull_request_number numeric(20, 0) NOT NULL,
    session_count bigint NOT NULL,

    PRIMARY KEY (repository, pull_request_number),
    CHECK (repo_watch_repository_is_valid(repository)),
    CHECK (pull_request_number > 0 AND pull_request_number <= 18446744073709551615),
    CHECK (session_count > 0)
);

INSERT INTO repo_watch_current_pull_request_session_count (
    repository, pull_request_number, session_count
)
SELECT correlated.repository, correlated.pull_request_number, count(*)
  FROM (
        SELECT action.repository, action.pull_request_number
          FROM repo_watch_dispatch_action AS action
         WHERE action.pull_request_number IS NOT NULL
        UNION ALL
        SELECT commissioned.repository, commissioned.pull_request_number
          FROM commissioned_dispatch AS commissioned
         WHERE commissioned.pull_request_number IS NOT NULL
  ) AS correlated
 GROUP BY correlated.repository, correlated.pull_request_number;

CREATE FUNCTION increment_repo_watch_pull_request_session_count()
RETURNS trigger
LANGUAGE plpgsql
SET search_path FROM CURRENT
AS $$
BEGIN
    IF NEW.pull_request_number IS NOT NULL THEN
        INSERT INTO repo_watch_current_pull_request_session_count (
            repository, pull_request_number, session_count
        ) VALUES (
            NEW.repository, NEW.pull_request_number, 1
        )
        ON CONFLICT (repository, pull_request_number) DO UPDATE
            SET session_count =
                repo_watch_current_pull_request_session_count.session_count + 1;
    END IF;
    RETURN NULL;
END;
$$;

CREATE TRIGGER repo_watch_dispatch_action_counts_pull_request_session
AFTER INSERT ON repo_watch_dispatch_action
FOR EACH ROW EXECUTE FUNCTION increment_repo_watch_pull_request_session_count();

CREATE TRIGGER commissioned_dispatch_counts_pull_request_session
AFTER INSERT ON commissioned_dispatch
FOR EACH ROW EXECUTE FUNCTION increment_repo_watch_pull_request_session_count();
