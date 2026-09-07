ALTER TABLE credential_pool_availability_successor
    DROP CONSTRAINT credential_pool_availability_successor_cause_kind_check;

ALTER TABLE credential_pool_availability_successor
    ADD CONSTRAINT credential_pool_availability_successor_cause_kind_check CHECK (
        cause_kind = ANY (ARRAY[
            'rate_limited'::text,
            'quota_exhausted'::text,
            'overloaded'::text,
            'provider_internal'::text
        ])
    );
