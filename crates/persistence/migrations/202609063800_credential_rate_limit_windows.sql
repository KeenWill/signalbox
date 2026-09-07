CREATE TABLE credential_rate_limit_snapshot (
    credential_reference text PRIMARY KEY,
    observation_model_call_id uuid NOT NULL REFERENCES model_call(model_call_id),
    -- Nanoseconds for a Duration's u64 seconds require at most 29 decimal digits.
    observed_at_nanos numeric(29, 0) NOT NULL,
    windows jsonb NOT NULL CHECK (jsonb_typeof(windows) = 'array')
);
