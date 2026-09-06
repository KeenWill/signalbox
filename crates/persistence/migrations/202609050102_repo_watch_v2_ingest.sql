-- Module tables are derived or module-local state. Each comment declares its
-- growth class and release condition; no retention duration or pruning pass is
-- selected here.

SET ROLE mod_repo_watch;
SET search_path = mod_repo_watch, pg_catalog;

-- growth: one mutable row per configured repository.
-- retention: delete when the repository leaves configuration.
CREATE TABLE repository_state (
    repository text PRIMARY KEY,
    default_branch text NOT NULL,
    default_head_sha text NOT NULL,
    observed_at timestamptz NOT NULL,
    updated_at timestamptz NOT NULL,
    CHECK (octet_length(repository) BETWEEN 1 AND 201),
    CHECK (octet_length(default_branch) BETWEEN 1 AND 255),
    CHECK (default_head_sha ~ '^[0-9a-f]{40}$')
);

-- growth: one mutable row per pull request in a configured repository.
-- retention: delete after its provider subject leaves the rebuild baseline.
CREATE TABLE pr_state (
    repository text NOT NULL,
    pull_request_number numeric(20,0) NOT NULL,
    lifecycle text NOT NULL,
    head_sha text NOT NULL,
    head_repository text NOT NULL,
    head_branch text NOT NULL,
    base_branch text NOT NULL,
    title text NOT NULL,
    body text NOT NULL,
    draft boolean NOT NULL,
    author text,
    observed_at timestamptz NOT NULL,
    updated_at timestamptz NOT NULL,
    PRIMARY KEY (repository, pull_request_number),
    FOREIGN KEY (repository) REFERENCES repository_state(repository) ON DELETE CASCADE,
    CHECK (pull_request_number BETWEEN 1 AND 18446744073709551615),
    CHECK (lifecycle = ANY (ARRAY['open', 'closed', 'merged'])),
    CHECK (head_sha ~ '^[0-9a-f]{40}$'),
    CHECK (octet_length(head_repository) BETWEEN 1 AND 201),
    CHECK (octet_length(head_branch) BETWEEN 1 AND 255),
    CHECK (octet_length(base_branch) BETWEEN 1 AND 255),
    CHECK (octet_length(title) BETWEEN 1 AND 1024),
    CHECK (octet_length(body) <= 262144),
    CHECK (author IS NULL OR octet_length(author) BETWEEN 1 AND 44)
);

-- growth: authenticated intake bounded by each row's expires_at.
-- retention: delete a delivery once expires_at is reached.
CREATE TABLE webhook_delivery (
    hook_id numeric(20,0) NOT NULL,
    delivery_id uuid NOT NULL,
    repository text NOT NULL,
    event_kind text NOT NULL,
    action text,
    body_digest bytea NOT NULL,
    received_at timestamptz NOT NULL,
    expires_at timestamptz NOT NULL,
    PRIMARY KEY (hook_id, delivery_id),
    CHECK (hook_id BETWEEN 1 AND 18446744073709551615),
    CHECK (octet_length(event_kind) BETWEEN 1 AND 128),
    CHECK (action IS NULL OR octet_length(action) BETWEEN 1 AND 128),
    CHECK (octet_length(body_digest) = 32),
    CHECK (expires_at > received_at)
);

-- growth: exactly one TTL-coupled body per retained delivery.
-- retention: cascade with webhook_delivery at expires_at.
CREATE TABLE webhook_body (
    hook_id numeric(20,0) NOT NULL,
    delivery_id uuid NOT NULL,
    body bytea NOT NULL,
    PRIMARY KEY (hook_id, delivery_id),
    FOREIGN KEY (hook_id, delivery_id)
        REFERENCES webhook_delivery(hook_id, delivery_id) ON DELETE CASCADE,
    CHECK (octet_length(body) <= 26214400)
);

-- growth: exactly one TTL-coupled disposition per retained delivery.
-- retention: cascade with webhook_delivery at expires_at.
CREATE TABLE webhook_disposition (
    hook_id numeric(20,0) NOT NULL,
    delivery_id uuid NOT NULL,
    disposition text NOT NULL,
    settled_at timestamptz,
    PRIMARY KEY (hook_id, delivery_id),
    FOREIGN KEY (hook_id, delivery_id)
        REFERENCES webhook_delivery(hook_id, delivery_id) ON DELETE CASCADE,
    CHECK (disposition = ANY (ARRAY['pending', 'applied', 'ignored', 'rejected'])),
    CHECK ((disposition = 'pending') = (settled_at IS NULL))
);

-- growth: fixed singleton mirroring module-state application progress.
-- retention: rebuilt from zero only as part of a complete schema rebuild.
CREATE TABLE core_event_cursor (
    singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
    applied_through numeric(20,0) NOT NULL,
    updated_at timestamptz NOT NULL,
    CHECK (applied_through BETWEEN 0 AND 18446744073709551615)
);

INSERT INTO core_event_cursor (applied_through, updated_at)
VALUES (0, statement_timestamp());

RESET search_path;
RESET ROLE;
