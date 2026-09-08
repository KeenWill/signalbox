ALTER TABLE tool_continuation_context_headroom
    ADD COLUMN pending_steering_content_bytes numeric(20, 0) NOT NULL DEFAULT 0
        CHECK (pending_steering_content_bytes >= 0),
    DROP CONSTRAINT tool_continuation_context_headroom_requires_compaction;

ALTER TABLE tool_continuation_context_headroom
    ADD CONSTRAINT tool_continuation_context_headroom_requires_compaction CHECK (
        usage_input_tokens
        + CASE WHEN usage_input_includes_cache_tokens THEN 0
               ELSE COALESCE(usage_cache_creation_input_tokens, 0)
                  + COALESCE(usage_cache_read_input_tokens, 0) END
        + COALESCE(usage_output_tokens, 0)
        + projected_result_content_bytes + pending_steering_content_bytes
        + max_output_tokens > context_window_tokens
    );
