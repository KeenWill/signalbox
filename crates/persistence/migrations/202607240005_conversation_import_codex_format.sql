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
