-- Admit Codex rollout snapshots without reinterpreting stored Claude Code
-- imports.

ALTER TABLE imported_conversation
    DROP CONSTRAINT imported_conversation_source_format_closed;

ALTER TABLE imported_conversation
    ADD CONSTRAINT imported_conversation_source_format_closed
        CHECK (
            source_format IN (
                'claude_code_session_jsonl',
                'codex_rollout_jsonl'
            )
        );

ALTER TABLE imported_conversation
    ADD CONSTRAINT imported_conversation_format_version_supported
        CHECK (
            (
                source_format = 'claude_code_session_jsonl'
                AND converter_version IN (1, 2)
            )
            OR (
                source_format = 'codex_rollout_jsonl'
                AND converter_version = 1
            )
        );
