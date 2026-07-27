-- Provider-reported token usage is terminal model-call evidence. Each count is
-- independently nullable because providers may omit any field; zero remains a
-- reported value rather than absence. Historical terminal calls remain
-- honestly unreported.

ALTER TABLE model_call
    ADD COLUMN usage_input_tokens numeric(20, 0),
    ADD COLUMN usage_output_tokens numeric(20, 0),
    ADD COLUMN usage_cache_creation_input_tokens numeric(20, 0),
    ADD COLUMN usage_cache_read_input_tokens numeric(20, 0),
    ADD CONSTRAINT model_call_usage_input_tokens_u64
        CHECK (
            usage_input_tokens IS NULL
            OR usage_input_tokens BETWEEN 0 AND 18446744073709551615
        ),
    ADD CONSTRAINT model_call_usage_output_tokens_u64
        CHECK (
            usage_output_tokens IS NULL
            OR usage_output_tokens BETWEEN 0 AND 18446744073709551615
        ),
    ADD CONSTRAINT model_call_usage_cache_creation_input_tokens_u64
        CHECK (
            usage_cache_creation_input_tokens IS NULL
            OR usage_cache_creation_input_tokens
                BETWEEN 0 AND 18446744073709551615
        ),
    ADD CONSTRAINT model_call_usage_cache_read_input_tokens_u64
        CHECK (
            usage_cache_read_input_tokens IS NULL
            OR usage_cache_read_input_tokens
                BETWEEN 0 AND 18446744073709551615
        ),
    ADD CONSTRAINT model_call_usage_is_terminal_evidence
        CHECK (
            state_kind = 'terminal'
            OR (
                usage_input_tokens IS NULL
                AND usage_output_tokens IS NULL
                AND usage_cache_creation_input_tokens IS NULL
                AND usage_cache_read_input_tokens IS NULL
            )
        );

-- The existing model_call_changes_are_guarded trigger makes every terminal row
-- immutable. Together with the terminal-only shape above, the four fields can
-- therefore be installed only by the same update that records the terminal
-- disposition and can never be corrected or cleared later.
