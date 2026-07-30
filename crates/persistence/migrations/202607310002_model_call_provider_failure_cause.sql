-- Retain only the adapter's closed provider-error classification. Provider
-- prose and native error material remain outside durable domain state. Nullable
-- preserves historical known-failed rows and non-provider known failures.
ALTER TABLE model_call
    ADD COLUMN terminal_provider_failure_cause text;

ALTER TABLE model_call
    ADD CONSTRAINT model_call_provider_failure_cause_closed
    CHECK (
        terminal_provider_failure_cause IS NULL
        OR terminal_provider_failure_cause IN (
            'credential_rejected',
            'permission_denied',
            'invalid_request',
            'target_not_found',
            'request_too_large',
            'rate_limited',
            'quota_exhausted',
            'overloaded',
            'provider_internal',
            'unrecognized'
        )
    ),
    ADD CONSTRAINT model_call_provider_failure_cause_requires_known_failure
    CHECK (
        terminal_provider_failure_cause IS NULL
        OR (
            state_kind = 'terminal'
            AND terminal_disposition_kind = 'known_failed'
        )
    );
