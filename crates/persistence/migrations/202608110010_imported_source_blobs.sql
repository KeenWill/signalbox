-- Pre-production reset: a database containing relational imported-source
-- bytes does not cross this schema boundary. The reset is conditional so a
-- database with no imported rows retains every unrelated durable record.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM imported_raw_source_record) THEN
        PERFORM set_config('session_replication_role', 'replica', true);
        TRUNCATE TABLE imported_conversation, imported_raw_source_record CASCADE;
        PERFORM set_config('session_replication_role', 'origin', true);
    END IF;
END;
$$;

ALTER TABLE imported_raw_source_record
    DROP COLUMN raw_bytes;

ALTER TABLE imported_raw_source_record
    ADD CONSTRAINT imported_raw_source_record_blob_fk
    FOREIGN KEY (content_hash)
    REFERENCES blob (digest);

-- The reset also retires the old display-title transition state. Every row in
-- the final schema is inserted with its resolved title facts, and the header
-- is append-only immediately after insertion.
ALTER TABLE imported_conversation
    ALTER COLUMN display_title_state DROP DEFAULT,
    DROP CONSTRAINT imported_conversation_display_title_state_closed,
    ADD CONSTRAINT imported_conversation_display_title_state_closed CHECK (
        display_title_state IN ('derived', 'underivable')
    );

DROP TRIGGER imported_conversation_is_append_only ON imported_conversation;
DROP FUNCTION reject_non_display_title_backfill_change();

CREATE FUNCTION reject_imported_conversation_change()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION 'imported_conversation is append-only'
        USING ERRCODE = '23514';
END;
$$;

CREATE TRIGGER imported_conversation_is_append_only
BEFORE UPDATE OR DELETE ON imported_conversation
FOR EACH ROW
EXECUTE FUNCTION reject_imported_conversation_change();
