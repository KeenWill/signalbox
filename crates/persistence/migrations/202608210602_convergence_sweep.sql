-- Durable state and typed audit for daemon-native pull-request convergence
-- reconciliation. The live reliability stack reserves 2026082106xx.

CREATE FUNCTION convergence_sweep_retry_budget()
RETURNS smallint
LANGUAGE sql
IMMUTABLE
PARALLEL SAFE
AS $$ SELECT 5::smallint $$;

DO $$
BEGIN
    EXECUTE format(
        'ALTER FUNCTION convergence_sweep_retry_budget() '
        'SET search_path TO %I, pg_catalog, pg_temp',
        current_schema
    );
END
$$;

CREATE INDEX commissioned_dispatch_pull_request_target
    ON commissioned_dispatch (
        target_kind, repository, pull_request_number, recorded_at DESC, dispatch_id DESC
    )
    WHERE target_kind = 'pull_request';

CREATE TABLE convergence_sweep_target (
    repository text NOT NULL,
    pull_request_number numeric(20, 0) NOT NULL,
    state_kind text NOT NULL DEFAULT 'observed',
    failure_kind text,
    consecutive_failures smallint NOT NULL DEFAULT 0,
    retry_not_before timestamptz,
    parked_at timestamptz,
    operator_need text,
    last_head_sha text,
    last_unresolved_threads numeric(20, 0),
    last_observed_at timestamptz,
    pending_command_id uuid,
    pending_head_sha text,
    pending_unresolved_threads numeric(20, 0),
    pending_content_digest bytea,
    pending_started_at timestamptz,
    last_dispatch_id uuid,
    last_session_id uuid,
    last_dispatched_at timestamptz,
    last_dispatch_head_sha text,
    last_dispatch_unresolved_threads numeric(20, 0),

    PRIMARY KEY (repository, pull_request_number),
    CHECK (repo_watch_repository_is_valid(repository)),
    CHECK (
        pull_request_number BETWEEN 1 AND 18446744073709551615
    ),
    CHECK (state_kind IN ('observed', 'retry_wait', 'parked')),
    CHECK (
        failure_kind IS NULL OR failure_kind IN (
            'facts_fetch', 'commission_refused', 'template_drift',
            'no_model_activity', 'state_access'
        )
    ),
    CHECK (
        consecutive_failures BETWEEN 0 AND convergence_sweep_retry_budget()
    ),
    CHECK (
        operator_need IS NULL OR operator_need IN (
            'repair_facts_fetch', 'repair_commission', 'repair_template',
            'inspect_inactive_session', 'repair_sweep_state'
        )
    ),
    CHECK (last_head_sha IS NULL OR last_head_sha COLLATE "C" ~ '^[0-9a-f]{40}$'),
    CHECK (pending_head_sha IS NULL OR pending_head_sha COLLATE "C" ~ '^[0-9a-f]{40}$'),
    CHECK (
        last_dispatch_head_sha IS NULL
        OR last_dispatch_head_sha COLLATE "C" ~ '^[0-9a-f]{40}$'
    ),
    CHECK (
        last_unresolved_threads IS NULL OR last_unresolved_threads >= 0
    ),
    CHECK (
        pending_unresolved_threads IS NULL OR pending_unresolved_threads >= 0
    ),
    CHECK (
        last_dispatch_unresolved_threads IS NULL
        OR last_dispatch_unresolved_threads >= 0
    ),
    CHECK (
        (state_kind = 'observed'
            AND failure_kind IS NULL
            AND consecutive_failures = 0
            AND retry_not_before IS NULL
            AND parked_at IS NULL
            AND operator_need IS NULL)
        OR
        (state_kind = 'retry_wait'
            AND failure_kind IS NOT NULL
            AND failure_kind <> 'no_model_activity'
            AND consecutive_failures BETWEEN 1
                AND convergence_sweep_retry_budget() - 1
            AND retry_not_before IS NOT NULL
            AND parked_at IS NULL
            AND operator_need IS NULL)
        OR
        (state_kind = 'parked'
            AND failure_kind IS NOT NULL
            AND consecutive_failures = convergence_sweep_retry_budget()
            AND retry_not_before IS NULL
            AND parked_at IS NOT NULL
            AND operator_need IS NOT NULL)
    ),
    CHECK (
        (pending_command_id IS NULL
            AND pending_head_sha IS NULL
            AND pending_unresolved_threads IS NULL
            AND pending_content_digest IS NULL
            AND pending_started_at IS NULL)
        OR
        (pending_command_id IS NOT NULL
            AND pending_head_sha IS NOT NULL
            AND pending_unresolved_threads IS NOT NULL
            AND pending_content_digest IS NOT NULL
            AND octet_length(pending_content_digest) = 32
            AND pending_started_at IS NOT NULL)
    ),
    CHECK (
        (last_dispatch_id IS NULL
            AND last_session_id IS NULL
            AND last_dispatched_at IS NULL
            AND last_dispatch_head_sha IS NULL
            AND last_dispatch_unresolved_threads IS NULL)
        OR
        (last_dispatch_id IS NOT NULL
            AND last_session_id IS NOT NULL
            AND last_dispatched_at IS NOT NULL
            AND last_dispatch_head_sha IS NOT NULL
            AND last_dispatch_unresolved_threads IS NOT NULL)
    ),
    FOREIGN KEY (last_dispatch_id, last_session_id)
        REFERENCES commissioned_dispatch (dispatch_id, session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT
);

CREATE TABLE convergence_sweep_event (
    event_id uuid PRIMARY KEY,
    repository text NOT NULL,
    pull_request_number numeric(20, 0) NOT NULL,
    outcome_kind text NOT NULL,
    failure_kind text,
    head_sha text,
    unresolved_threads numeric(20, 0),
    dispatch_id uuid,
    session_id uuid,
    consecutive_failures smallint NOT NULL,
    retry_not_before timestamptz,
    operator_need text,
    recorded_at timestamptz NOT NULL DEFAULT transaction_timestamp(),

    FOREIGN KEY (repository, pull_request_number)
        REFERENCES convergence_sweep_target (repository, pull_request_number)
        ON UPDATE RESTRICT ON DELETE RESTRICT,
    FOREIGN KEY (dispatch_id, session_id)
        REFERENCES commissioned_dispatch (dispatch_id, session_id)
        ON UPDATE RESTRICT ON DELETE RESTRICT,
    CHECK (repo_watch_repository_is_valid(repository)),
    CHECK (pull_request_number BETWEEN 1 AND 18446744073709551615),
    CHECK (
        outcome_kind IN (
            'converged', 'cooling_off', 'live_session', 'dispatched',
            'facts_fetch_failed', 'commission_refused', 'template_drift',
            'no_model_activity', 'state_access_failed'
        )
    ),
    CHECK (
        failure_kind IS NULL OR failure_kind IN (
            'facts_fetch', 'commission_refused', 'template_drift',
            'no_model_activity', 'state_access'
        )
    ),
    CHECK (head_sha IS NULL OR head_sha COLLATE "C" ~ '^[0-9a-f]{40}$'),
    CHECK (unresolved_threads IS NULL OR unresolved_threads >= 0),
    CHECK (
        consecutive_failures BETWEEN 0 AND convergence_sweep_retry_budget()
    ),
    CHECK (
        operator_need IS NULL OR operator_need IN (
            'repair_facts_fetch', 'repair_commission', 'repair_template',
            'inspect_inactive_session', 'repair_sweep_state'
        )
    ),
    CHECK ((dispatch_id IS NULL) = (session_id IS NULL)),
    CHECK (
        (outcome_kind IN ('converged', 'cooling_off', 'live_session', 'dispatched')
            AND failure_kind IS NULL
            AND consecutive_failures = 0
            AND retry_not_before IS NULL
            AND operator_need IS NULL)
        OR
        (outcome_kind = 'facts_fetch_failed'
            AND failure_kind = 'facts_fetch')
        OR
        (outcome_kind = 'commission_refused'
            AND failure_kind = 'commission_refused')
        OR
        (outcome_kind = 'template_drift'
            AND failure_kind = 'template_drift')
        OR
        (outcome_kind = 'no_model_activity'
            AND failure_kind = 'no_model_activity')
        OR
        (outcome_kind = 'state_access_failed'
            AND failure_kind = 'state_access')
    ),
    CHECK (
        failure_kind IS NULL
        OR
        (operator_need IS NULL
            AND consecutive_failures BETWEEN 1
                AND convergence_sweep_retry_budget() - 1
            AND retry_not_before IS NOT NULL)
        OR
        (operator_need IS NOT NULL
            AND consecutive_failures = convergence_sweep_retry_budget()
            AND retry_not_before IS NULL)
    )
);

CREATE TRIGGER convergence_sweep_event_is_append_only
BEFORE UPDATE OR DELETE ON convergence_sweep_event
FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER convergence_sweep_event_reject_truncate
BEFORE TRUNCATE ON convergence_sweep_event
FOR EACH STATEMENT EXECUTE FUNCTION reject_repo_watch_table_truncate();

CREATE VIEW convergence_sweep_parked_target AS
SELECT repository,
       pull_request_number,
       failure_kind,
       consecutive_failures,
       parked_at,
       operator_need,
       last_head_sha,
       last_unresolved_threads,
       last_dispatch_id,
       last_session_id,
       last_dispatched_at
  FROM convergence_sweep_target
 WHERE state_kind = 'parked';
