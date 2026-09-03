-- Durable turn-watchdog observations and config-fed recovery ceilings.

CREATE TABLE turn_liveness_observation (
    guard_kind text NOT NULL,
    turn_id uuid NOT NULL,
    session_id uuid NOT NULL,
    current_attempt_id uuid NOT NULL,
    outbox_frontier_token text NOT NULL,
    scan_interval_seconds numeric(20, 0) NOT NULL,
    scan_interval_subsec_nanos integer NOT NULL,
    observation_ordinal bigint NOT NULL,
    recorded_at timestamp with time zone DEFAULT statement_timestamp() NOT NULL,
    CONSTRAINT turn_liveness_observation_pkey PRIMARY KEY (guard_kind, turn_id),
    CONSTRAINT turn_liveness_observation_guard_kind CHECK (
        guard_kind = ANY (ARRAY['quiescent'::text, 'slot_held'::text])
    ),
    CONSTRAINT turn_liveness_observation_frontier CHECK (
        outbox_frontier_token = 'none'
        OR outbox_frontier_token ~ '^[0-9]+$'
    ),
    CONSTRAINT turn_liveness_observation_ordinal CHECK (observation_ordinal > 0),
    CONSTRAINT turn_liveness_observation_scan_interval CHECK (
        scan_interval_seconds >= 0
        AND scan_interval_seconds <= 18446744073709551615
        AND scan_interval_subsec_nanos >= 0
        AND scan_interval_subsec_nanos < 1000000000
        AND (scan_interval_seconds > 0 OR scan_interval_subsec_nanos > 0)
    ),
    CONSTRAINT turn_liveness_observation_turn_fkey
        FOREIGN KEY (turn_id) REFERENCES turn_lifecycle(turn_id),
    CONSTRAINT turn_liveness_observation_session_fkey
        FOREIGN KEY (session_id) REFERENCES session(session_id),
    CONSTRAINT turn_liveness_observation_attempt_fkey
        FOREIGN KEY (current_attempt_id) REFERENCES turn_attempt(turn_attempt_id)
);

-- Supersedes the definition in 202609010002_turns.
ALTER TABLE automatic_reconciliation
    DROP CONSTRAINT automatic_model_call_reconciliation_attempt_count,
    ADD COLUMN attempt_ceiling integer NOT NULL,
    ADD CONSTRAINT automatic_reconciliation_attempt_ceiling CHECK (attempt_ceiling > 0),
    ADD CONSTRAINT automatic_reconciliation_attempt_count CHECK (
        attempt_count >= 0 AND attempt_count <= attempt_ceiling
    );

-- Supersedes the definition in 202609010002_turns.
ALTER TABLE automatic_reconciliation_attempt
    DROP CONSTRAINT automatic_model_call_reconciliation_attempt_ordinal,
    ADD COLUMN attempt_ceiling integer NOT NULL,
    ADD CONSTRAINT automatic_reconciliation_attempt_ceiling CHECK (attempt_ceiling > 0),
    ADD CONSTRAINT automatic_reconciliation_attempt_ordinal CHECK (
        attempt_ordinal >= 1 AND attempt_ordinal <= attempt_ceiling
    );

-- Supersedes the definitions in 202609010009_repo_watch.
ALTER TABLE convergence_sweep_event
    DROP CONSTRAINT convergence_sweep_event_check2,
    DROP CONSTRAINT convergence_sweep_event_consecutive_failures_check,
    ADD COLUMN retry_budget smallint NOT NULL,
    ADD CONSTRAINT convergence_sweep_event_retry_budget CHECK (retry_budget > 0),
    ADD CONSTRAINT convergence_sweep_event_consecutive_failures CHECK (
        consecutive_failures >= 0 AND consecutive_failures <= retry_budget
    ),
    ADD CONSTRAINT convergence_sweep_event_failure_shape CHECK (
        failure_kind IS NULL
        OR (
            operator_need IS NULL
            AND consecutive_failures >= 1
            AND consecutive_failures < retry_budget
            AND retry_not_before IS NOT NULL
        )
        OR (
            operator_need IS NOT NULL
            AND consecutive_failures = retry_budget
            AND retry_not_before IS NULL
        )
    );

-- Supersedes the definitions in 202609010009_repo_watch.
ALTER TABLE convergence_sweep_target
    DROP CONSTRAINT convergence_sweep_target_check,
    DROP CONSTRAINT convergence_sweep_target_consecutive_failures_check,
    ADD COLUMN retry_budget smallint NOT NULL,
    ADD CONSTRAINT convergence_sweep_target_retry_budget CHECK (retry_budget > 0),
    ADD CONSTRAINT convergence_sweep_target_consecutive_failures CHECK (
        consecutive_failures >= 0 AND consecutive_failures <= retry_budget
    ),
    ADD CONSTRAINT convergence_sweep_target_shape CHECK (
        (
            state_kind = 'observed'
            AND failure_kind IS NULL
            AND consecutive_failures = 0
            AND retry_not_before IS NULL
            AND parked_at IS NULL
            AND operator_need IS NULL
        )
        OR (
            state_kind = 'retry_wait'
            AND failure_kind IS NOT NULL
            AND failure_kind <> 'no_model_activity'
            AND consecutive_failures >= 1
            AND consecutive_failures < retry_budget
            AND retry_not_before IS NOT NULL
            AND parked_at IS NULL
            AND operator_need IS NULL
        )
        OR (
            state_kind = 'parked'
            AND failure_kind IS NOT NULL
            AND consecutive_failures = retry_budget
            AND retry_not_before IS NULL
            AND parked_at IS NOT NULL
            AND operator_need IS NOT NULL
        )
    );

DROP FUNCTION convergence_sweep_retry_budget();
