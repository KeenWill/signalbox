-- Durable turn-watchdog observations.

CREATE TABLE turn_liveness_observation (
    guard_kind text NOT NULL,
    turn_id uuid NOT NULL,
    session_id uuid NOT NULL,
    current_attempt_id uuid NOT NULL,
    outbox_frontier numeric(20, 0),
    scan_interval_seconds numeric(20, 0) NOT NULL,
    observation_ordinal bigint NOT NULL,
    CONSTRAINT turn_liveness_observation_pkey PRIMARY KEY (guard_kind, turn_id),
    CONSTRAINT turn_liveness_observation_guard_kind CHECK (
        guard_kind = ANY (ARRAY['quiescent'::text, 'slot_held'::text])
    ),
    CONSTRAINT turn_liveness_observation_frontier CHECK (
        outbox_frontier IS NULL
        OR (
            outbox_frontier >= 0
            AND outbox_frontier <= 18446744073709551615
        )
    ),
    CONSTRAINT turn_liveness_observation_ordinal CHECK (observation_ordinal > 0),
    CONSTRAINT turn_liveness_observation_scan_interval CHECK (
        scan_interval_seconds > 0
        AND scan_interval_seconds <= 18446744073709551615
    ),
    CONSTRAINT turn_liveness_observation_turn_fkey
        FOREIGN KEY (turn_id) REFERENCES turn_lifecycle(turn_id),
    CONSTRAINT turn_liveness_observation_session_fkey
        FOREIGN KEY (session_id) REFERENCES session(session_id),
    CONSTRAINT turn_liveness_observation_attempt_fkey
        FOREIGN KEY (current_attempt_id) REFERENCES turn_attempt(turn_attempt_id)
);
