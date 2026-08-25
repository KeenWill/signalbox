-- Bound repository webhook health windows by their authoritative receipt time.

CREATE INDEX repo_watch_webhook_delivery_repository_received_at
    ON repo_watch_webhook_delivery (repository, received_at DESC);
