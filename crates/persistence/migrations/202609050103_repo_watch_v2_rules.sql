-- Module tables are derived or module-local state. Each comment declares its
-- growth class and release condition; no retention duration or pruning pass is
-- selected here.

SET ROLE mod_repo_watch;
SET search_path = mod_repo_watch, pg_catalog;

-- growth: one mutable row per recurring event-identity stream.
-- retention: delete a pull-request stream when that provider subject leaves the rebuild baseline.
CREATE TABLE frontier (
    repository text NOT NULL,
    stream_identity bytea NOT NULL,
    sequence numeric(20,0) NOT NULL,
    pull_request_number numeric(20,0),
    updated_at timestamptz NOT NULL,
    PRIMARY KEY (repository, stream_identity),
    FOREIGN KEY (repository) REFERENCES repository_state(repository) ON DELETE CASCADE,
    CHECK (octet_length(stream_identity) = 32),
    CHECK (sequence BETWEEN 1 AND 18446744073709551615),
    CHECK (pull_request_number IS NULL OR pull_request_number BETWEEN 1 AND 18446744073709551615)
);

-- growth: append-only facts bounded by each row's retain_until.
-- retention: delete an event once retain_until is reached and no pending module command names it.
CREATE TABLE gh_event (
    event_id uuid PRIMARY KEY,
    content_identity bytea NOT NULL UNIQUE,
    repository text NOT NULL,
    event_kind text NOT NULL,
    target_kind text NOT NULL,
    pull_request_number numeric(20,0),
    normalized_payload bytea NOT NULL,
    recorded_at timestamptz NOT NULL,
    retain_until timestamptz NOT NULL,
    FOREIGN KEY (repository) REFERENCES repository_state(repository) ON DELETE CASCADE,
    CHECK (octet_length(content_identity) = 32),
    CHECK (event_kind = ANY (ARRAY[
        'pull_request_opened', 'pull_request_closed', 'pull_request_merged',
        'head_changed', 'mergeable_state_changed', 'checks_completed',
        'check_run_completed', 'branch_workflow_run_completed', 'review_submitted',
        'thread_opened', 'thread_resolved', 'labeled', 'unlabeled',
        'base_advanced', 'reaction_changed'
    ])),
    CHECK (target_kind = ANY (ARRAY['pull_request', 'branch'])),
    CHECK ((target_kind = 'pull_request') = (pull_request_number IS NOT NULL)),
    CHECK (pull_request_number IS NULL OR pull_request_number BETWEEN 1 AND 18446744073709551615),
    CHECK (retain_until > recorded_at)
);

-- growth: one row per configured rule revision.
-- retention: delete an inactive revision once no retained dispatch names it.
CREATE TABLE rule_revision (
    repository text NOT NULL,
    rule_id text NOT NULL,
    revision numeric(20,0) NOT NULL,
    content_digest bytea NOT NULL,
    activated_at timestamptz NOT NULL,
    retired_at timestamptz,
    PRIMARY KEY (repository, rule_id, revision),
    CHECK (octet_length(rule_id) BETWEEN 1 AND 128),
    CHECK (revision BETWEEN 1 AND 9223372036854775807),
    CHECK (octet_length(content_digest) = 32),
    CHECK (retired_at IS NULL OR retired_at >= activated_at)
);

-- growth: one row per identity-relevant field of a configured rule revision.
-- retention: delete with the inactive revision once no retained dispatch names it.
CREATE TABLE rule_field_fingerprint (
    repository text NOT NULL,
    rule_id text NOT NULL,
    revision numeric(20,0) NOT NULL,
    field_ordinal smallint NOT NULL,
    field_name text NOT NULL,
    field_digest bytea NOT NULL,
    PRIMARY KEY (repository, rule_id, revision, field_ordinal),
    FOREIGN KEY (repository, rule_id, revision)
        REFERENCES rule_revision(repository, rule_id, revision) ON DELETE CASCADE,
    CHECK (field_ordinal >= 0),
    CHECK (octet_length(field_name) BETWEEN 1 AND 128),
    CHECK (octet_length(field_digest) = 32)
);

-- growth: one mutable row per configured rule in a watched repository.
-- retention: delete when the rule leaves configuration; revision history remains.
CREATE TABLE rule (
    repository text NOT NULL,
    rule_id text NOT NULL,
    active_revision numeric(20,0) NOT NULL,
    content_digest bytea NOT NULL,
    updated_at timestamptz NOT NULL,
    PRIMARY KEY (repository, rule_id),
    FOREIGN KEY (repository, rule_id, active_revision)
        REFERENCES rule_revision(repository, rule_id, revision),
    CHECK (octet_length(rule_id) BETWEEN 1 AND 128),
    CHECK (active_revision BETWEEN 1 AND 9223372036854775807),
    CHECK (octet_length(content_digest) = 32)
);

RESET search_path;
RESET ROLE;
