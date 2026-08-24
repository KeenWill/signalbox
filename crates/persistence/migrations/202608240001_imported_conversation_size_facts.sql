-- Persist immutable descriptor size facts at ingestion so bounded discovery
-- does not rescan every raw occurrence and transcript entry.

ALTER TABLE imported_conversation
    ADD COLUMN raw_source_bytes numeric(20, 0),
    ADD COLUMN normalized_source_record_bytes numeric(20, 0),
    ADD COLUMN normalized_entry_bytes numeric(20, 0);

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
