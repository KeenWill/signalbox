CREATE TABLE credential_rate_limit_snapshot (
    credential_reference text PRIMARY KEY,
    observation_model_call_id uuid NOT NULL REFERENCES model_call(model_call_id),
    observed_at timestamp with time zone NOT NULL,
    windows jsonb NOT NULL CHECK (jsonb_typeof(windows) = 'array')
);
