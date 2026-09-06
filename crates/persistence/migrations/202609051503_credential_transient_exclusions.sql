CREATE TABLE credential_pool_transient_exclusion (
    observation_model_call_id uuid PRIMARY KEY
        REFERENCES model_call(model_call_id),
    credential_reference text NOT NULL,
    cause_kind text NOT NULL CHECK (
        cause_kind = ANY (ARRAY[
            'rate_limited'::text,
            'overloaded'::text,
            'provider_internal'::text
        ])
    ),
    reset_at timestamp with time zone NOT NULL
);

CREATE INDEX credential_pool_transient_exclusion_active_idx
    ON credential_pool_transient_exclusion
    (credential_reference, reset_at);

ALTER TABLE credential_pool_availability_successor
    DROP CONSTRAINT credential_pool_availability_successor_cause_kind_check;

ALTER TABLE credential_pool_availability_successor
    ADD CONSTRAINT credential_pool_availability_successor_cause_kind_check CHECK (
        cause_kind = ANY (ARRAY[
            'rate_limited'::text,
            'quota_exhausted'::text,
            'overloaded'::text,
            'provider_internal'::text,
            'credential_rejected'::text
        ])
    );

ALTER TABLE credential_pool_chain_exclusion
    DROP CONSTRAINT credential_pool_chain_exclusion_cause_kind_check;

ALTER TABLE credential_pool_chain_exclusion
    ADD CONSTRAINT credential_pool_chain_exclusion_cause_kind_check CHECK (
        cause_kind = ANY (ARRAY[
            'rate_limited'::text,
            'quota_exhausted'::text,
            'overloaded'::text,
            'credential_rejected'::text
        ])
    );

ALTER TABLE credential_pool_terminal_exhaustion
    DROP CONSTRAINT credential_pool_terminal_exhaustion_cause_kind_check;

ALTER TABLE credential_pool_terminal_exhaustion
    ADD CONSTRAINT credential_pool_terminal_exhaustion_cause_kind_check CHECK (
        cause_kind = ANY (ARRAY[
            'rate_limited'::text,
            'quota_exhausted'::text,
            'overloaded'::text,
            'credential_rejected'::text
        ])
    );
