-- Tool results are appended after the producing call reports its input usage.
-- Preserve their bounded UTF-8 payload size as a conservative continuation
-- allowance so a large result batch cannot bypass the context-headroom guard.

ALTER TABLE tool_continuation_context_headroom
    ADD COLUMN projected_result_content_bytes numeric(20, 0) NOT NULL DEFAULT 0;

ALTER TABLE tool_continuation_context_headroom
    ALTER COLUMN projected_result_content_bytes DROP DEFAULT,
    ADD CONSTRAINT tool_continuation_context_headroom_result_bytes_nonnegative
        CHECK (projected_result_content_bytes >= 0),
    DROP CONSTRAINT tool_continuation_context_headroom_proves_exhaustion,
    ADD CONSTRAINT tool_continuation_context_headroom_requires_compaction CHECK (
        (
            usage_input_tokens
            + CASE
                WHEN usage_input_includes_cache_tokens THEN 0
                ELSE COALESCE(usage_cache_creation_input_tokens, 0)
                    + COALESCE(usage_cache_read_input_tokens, 0)
              END
            + COALESCE(usage_output_tokens, 0)
            + projected_result_content_bytes
            + max_output_tokens
        ) > context_window_tokens
    );
