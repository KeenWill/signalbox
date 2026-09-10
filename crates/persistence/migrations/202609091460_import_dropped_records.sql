ALTER TABLE imported_conversation
    ADD COLUMN dropped_record_count numeric(20,0),
    ADD COLUMN first_dropped_record_position numeric(20,0);

UPDATE imported_conversation
SET dropped_record_count = 0;

ALTER TABLE imported_conversation
    ALTER COLUMN dropped_record_count SET NOT NULL,
    DROP CONSTRAINT imported_conversation_converter_version_supported,
    ADD CONSTRAINT imported_conversation_converter_version_supported
        CHECK (converter_version = ANY (ARRAY[1, 2, 3])),
    DROP CONSTRAINT imported_conversation_format_version_supported,
    ADD CONSTRAINT imported_conversation_format_version_supported
        CHECK (
            (source_format = 'claude_code_session_jsonl' AND converter_version = ANY (ARRAY[1, 2, 3]))
            OR (source_format = 'codex_rollout_jsonl' AND converter_version = ANY (ARRAY[1, 2]))
        ),
    ADD CONSTRAINT imported_conversation_dropped_record_count_u64
        CHECK (dropped_record_count BETWEEN 0 AND 18446744073709551615),
    ADD CONSTRAINT imported_conversation_first_dropped_position_positive_u64
        CHECK (first_dropped_record_position BETWEEN 1 AND 18446744073709551615),
    ADD CONSTRAINT imported_conversation_dropped_record_facts_complete
        CHECK ((dropped_record_count = 0) = (first_dropped_record_position IS NULL));
