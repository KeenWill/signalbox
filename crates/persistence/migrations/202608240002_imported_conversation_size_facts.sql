-- Persist immutable descriptor size facts at ingestion so bounded discovery
-- does not rescan every raw occurrence and transcript entry.

ALTER TABLE imported_conversation
    ADD COLUMN raw_source_bytes numeric(20, 0),
    ADD COLUMN normalized_source_record_bytes numeric(20, 0),
    ADD COLUMN normalized_entry_bytes numeric(20, 0);

-- Persist the bounded discovery discriminator independently of payload size.
-- The length guard preserves fail-closed reads for malformed short encodings.
ALTER TABLE imported_transcript_entry
    ADD COLUMN content_kind text GENERATED ALWAYS AS (
        CASE WHEN octet_length(content_encoding) >= 3 THEN
            CASE get_byte(content_encoding, 2)
                WHEN 0 THEN 'source_event'
                WHEN 1 THEN 'text'
                WHEN 2 THEN 'tool_call'
                WHEN 3 THEN 'tool_result'
                WHEN 4 THEN 'thinking'
                WHEN 5 THEN 'redacted_thinking'
                WHEN 6 THEN 'document'
                WHEN 7 THEN 'message_content_absent'
                WHEN 8 THEN 'source_message_block'
                ELSE NULL
            END
        END
    ) STORED,
    ADD CONSTRAINT imported_transcript_entry_content_kind_closed CHECK (
        content_kind IN (
            'source_event',
            'text',
            'tool_call',
            'tool_result',
            'thinking',
            'redacted_thinking',
            'document',
            'message_content_absent',
            'source_message_block'
        )
    );

ALTER TABLE imported_conversation DISABLE TRIGGER USER;

UPDATE imported_conversation AS imported
   SET raw_source_bytes = (
           SELECT COALESCE(SUM(octet_length(blob.raw_bytes)), 0)::numeric
             FROM imported_conversation_raw_record AS occurrence
             JOIN imported_raw_source_record AS blob
               ON blob.content_hash = occurrence.content_hash
            WHERE occurrence.imported_conversation_id = imported.imported_conversation_id
       ),
       normalized_source_record_bytes = (
           SELECT COALESCE(SUM(octet_length(occurrence.normalized_value_encoding)), 0)::numeric
             FROM imported_conversation_raw_record AS occurrence
            WHERE occurrence.imported_conversation_id = imported.imported_conversation_id
       ),
       normalized_entry_bytes = (
           SELECT COALESCE(SUM(
               octet_length(entry.content_encoding)
               + octet_length(entry.source_metadata_encoding)
           ), 0)::numeric
             FROM imported_transcript_entry AS entry
            WHERE entry.imported_conversation_id = imported.imported_conversation_id
       );

ALTER TABLE imported_conversation ENABLE TRIGGER USER;

ALTER TABLE imported_conversation
    ALTER COLUMN raw_source_bytes SET NOT NULL,
    ALTER COLUMN normalized_source_record_bytes SET NOT NULL,
    ALTER COLUMN normalized_entry_bytes SET NOT NULL,
    ADD CONSTRAINT imported_conversation_raw_source_bytes_u64
        CHECK (raw_source_bytes >= 0 AND raw_source_bytes <= 18446744073709551615),
    ADD CONSTRAINT imported_conversation_normalized_source_record_bytes_u64
        CHECK (
            normalized_source_record_bytes >= 0
            AND normalized_source_record_bytes <= 18446744073709551615
        ),
    ADD CONSTRAINT imported_conversation_normalized_entry_bytes_u64
        CHECK (
            normalized_entry_bytes >= 0
            AND normalized_entry_bytes <= 18446744073709551615
        );

CREATE OR REPLACE FUNCTION reject_non_display_title_backfill_change()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF TG_OP = 'UPDATE'
        AND OLD.display_title_state = 'pending'
        AND NEW.display_title_state IN ('derived', 'underivable')
        AND NEW.imported_conversation_id = OLD.imported_conversation_id
        AND NEW.storage_version = OLD.storage_version
        AND NEW.source_format = OLD.source_format
        AND NEW.converter_version = OLD.converter_version
        AND NEW.source_digest = OLD.source_digest
        AND NEW.source_session_id IS NOT DISTINCT FROM OLD.source_session_id
        AND NEW.declared_raw_record_count = OLD.declared_raw_record_count
        AND NEW.declared_entry_count = OLD.declared_entry_count
        AND NEW.raw_source_bytes = OLD.raw_source_bytes
        AND NEW.normalized_source_record_bytes = OLD.normalized_source_record_bytes
        AND NEW.normalized_entry_bytes = OLD.normalized_entry_bytes
    THEN
        RETURN NEW;
    END IF;
    RAISE EXCEPTION
        '% admits only the pending display-title backfill update', TG_TABLE_NAME
        USING ERRCODE = '23514';
END;
$$;
