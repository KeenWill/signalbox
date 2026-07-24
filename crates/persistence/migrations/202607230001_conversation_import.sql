-- Immutable imported-conversation snapshots and globally deduplicated raw
-- source records. Import is pure ingestion and writes no native session,
-- command, scheduler, execution, or outbox record.

CREATE TABLE imported_raw_source_record (
    content_hash bytea PRIMARY KEY,
    raw_bytes bytea NOT NULL,

    CONSTRAINT imported_raw_source_record_hash_size
        CHECK (octet_length(content_hash) = 32),
    CONSTRAINT imported_raw_source_record_bytes_nonempty
        CHECK (octet_length(raw_bytes) >= 1)
);

CREATE TABLE imported_conversation (
    imported_conversation_id uuid PRIMARY KEY,
    storage_version smallint NOT NULL,
    source_format text NOT NULL,
    converter_version smallint NOT NULL,
    source_digest bytea NOT NULL,
    declared_raw_record_count numeric(20, 0) NOT NULL,
    declared_entry_count numeric(20, 0) NOT NULL,

    CONSTRAINT imported_conversation_storage_version_supported
        CHECK (storage_version = 1),
    CONSTRAINT imported_conversation_source_format_closed
        CHECK (source_format = 'claude_code_session_jsonl'),
    CONSTRAINT imported_conversation_converter_version_supported
        CHECK (converter_version = 1),
    CONSTRAINT imported_conversation_source_digest_size
        CHECK (octet_length(source_digest) = 32),
    CONSTRAINT imported_conversation_raw_record_count_positive_u64
        CHECK (
            declared_raw_record_count >= 1
            AND declared_raw_record_count <= 18446744073709551615
        ),
    CONSTRAINT imported_conversation_entry_count_positive_u64
        CHECK (
            declared_entry_count >= 1
            AND declared_entry_count <= 18446744073709551615
        ),
    CONSTRAINT imported_conversation_source_identity
        UNIQUE (source_format, converter_version, source_digest)
);

CREATE TABLE imported_conversation_raw_record (
    imported_conversation_id uuid NOT NULL,
    raw_record_position numeric(20, 0) NOT NULL,
    content_hash bytea NOT NULL,
    conversion_digest bytea NOT NULL,
    normalized_value_encoding bytea NOT NULL,
    declared_entry_count numeric(20, 0) NOT NULL,

    CONSTRAINT imported_conversation_raw_record_pk
        PRIMARY KEY (imported_conversation_id, raw_record_position),
    CONSTRAINT imported_conversation_raw_record_position_positive_u64
        CHECK (
            raw_record_position >= 1
            AND raw_record_position <= 18446744073709551615
        ),
    CONSTRAINT imported_conversation_raw_record_entry_count_positive_u64
        CHECK (
            declared_entry_count >= 1
            AND declared_entry_count <= 18446744073709551615
        ),
    CONSTRAINT imported_conversation_raw_record_hash_size
        CHECK (octet_length(content_hash) = 32),
    CONSTRAINT imported_conversation_raw_record_conversion_digest_size
        CHECK (octet_length(conversion_digest) = 32),
    CONSTRAINT imported_conversation_raw_record_encoding_nonempty
        CHECK (octet_length(normalized_value_encoding) >= 1),
    CONSTRAINT imported_conversation_raw_record_owner_fk
        FOREIGN KEY (imported_conversation_id)
        REFERENCES imported_conversation (imported_conversation_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT imported_conversation_raw_record_blob_fk
        FOREIGN KEY (content_hash)
        REFERENCES imported_raw_source_record (content_hash)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE TABLE imported_transcript_entry (
    imported_conversation_id uuid NOT NULL,
    imported_entry_position numeric(20, 0) NOT NULL,
    imported_transcript_entry_id uuid NOT NULL,
    raw_record_position numeric(20, 0) NOT NULL,
    record_entry_position numeric(20, 0) NOT NULL,
    source_speaker_kind text NOT NULL,
    content_encoding bytea NOT NULL,
    source_metadata_encoding bytea NOT NULL,

    CONSTRAINT imported_transcript_entry_pk
        PRIMARY KEY (imported_conversation_id, imported_entry_position),
    CONSTRAINT imported_transcript_entry_identity_unique
        UNIQUE (imported_transcript_entry_id),
    CONSTRAINT imported_transcript_entry_within_record_unique
        UNIQUE (
            imported_conversation_id,
            raw_record_position,
            record_entry_position
        ),
    CONSTRAINT imported_transcript_entry_position_positive_u64
        CHECK (
            imported_entry_position >= 1
            AND imported_entry_position <= 18446744073709551615
        ),
    CONSTRAINT imported_transcript_entry_raw_position_positive_u64
        CHECK (
            raw_record_position >= 1
            AND raw_record_position <= 18446744073709551615
        ),
    CONSTRAINT imported_transcript_entry_record_position_positive_u64
        CHECK (
            record_entry_position >= 1
            AND record_entry_position <= 18446744073709551615
        ),
    CONSTRAINT imported_transcript_entry_source_speaker_closed
        CHECK (
            source_speaker_kind IN (
                'not_attested',
                'attested_absent',
                'attested_user',
                'attested_assistant'
            )
        ),
    CONSTRAINT imported_transcript_entry_content_encoding_nonempty
        CHECK (octet_length(content_encoding) >= 1),
    CONSTRAINT imported_transcript_entry_source_encoding_nonempty
        CHECK (octet_length(source_metadata_encoding) >= 1),
    CONSTRAINT imported_transcript_entry_owner_fk
        FOREIGN KEY (imported_conversation_id)
        REFERENCES imported_conversation (imported_conversation_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT imported_transcript_entry_raw_record_fk
        FOREIGN KEY (imported_conversation_id, raw_record_position)
        REFERENCES imported_conversation_raw_record (
            imported_conversation_id,
            raw_record_position
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE FUNCTION require_imported_raw_record_within_declared_count()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    declared_count numeric(20, 0);
BEGIN
    SELECT declared_raw_record_count
      INTO declared_count
      FROM imported_conversation
     WHERE imported_conversation_id = NEW.imported_conversation_id;

    IF NOT FOUND OR NEW.raw_record_position > declared_count THEN
        RAISE EXCEPTION
            'imported raw-record position is outside its declared conversation'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER imported_raw_record_stays_within_declared_count
BEFORE INSERT ON imported_conversation_raw_record
FOR EACH ROW
EXECUTE FUNCTION require_imported_raw_record_within_declared_count();

CREATE FUNCTION require_imported_entry_within_declared_counts()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    conversation_count numeric(20, 0);
    raw_entry_count numeric(20, 0);
BEGIN
    SELECT declared_entry_count
      INTO conversation_count
      FROM imported_conversation
     WHERE imported_conversation_id = NEW.imported_conversation_id;

    SELECT declared_entry_count
      INTO raw_entry_count
      FROM imported_conversation_raw_record
     WHERE imported_conversation_id = NEW.imported_conversation_id
       AND raw_record_position = NEW.raw_record_position;

    IF conversation_count IS NULL
       OR raw_entry_count IS NULL
       OR NEW.imported_entry_position > conversation_count
       OR NEW.record_entry_position > raw_entry_count
    THEN
        RAISE EXCEPTION
            'imported entry position is outside its declared conversation or raw record'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER imported_entry_stays_within_declared_counts
BEFORE INSERT ON imported_transcript_entry
FOR EACH ROW
EXECUTE FUNCTION require_imported_entry_within_declared_counts();

CREATE FUNCTION assert_imported_conversation_complete(
    checked_imported_conversation_id uuid
)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    expected_raw_count numeric(20, 0);
    expected_entry_count numeric(20, 0);
    actual_raw_count numeric(20, 0);
    first_raw_position numeric(20, 0);
    last_raw_position numeric(20, 0);
    actual_entry_count numeric(20, 0);
    first_entry_position numeric(20, 0);
    last_entry_position numeric(20, 0);
BEGIN
    SELECT declared_raw_record_count, declared_entry_count
      INTO expected_raw_count, expected_entry_count
      FROM imported_conversation
     WHERE imported_conversation_id = checked_imported_conversation_id;

    IF NOT FOUND THEN
        RETURN;
    END IF;

    SELECT count(*)::numeric(20, 0),
           min(raw_record_position),
           max(raw_record_position)
      INTO actual_raw_count, first_raw_position, last_raw_position
      FROM imported_conversation_raw_record
     WHERE imported_conversation_id = checked_imported_conversation_id;

    SELECT count(*)::numeric(20, 0),
           min(imported_entry_position),
           max(imported_entry_position)
      INTO actual_entry_count, first_entry_position, last_entry_position
      FROM imported_transcript_entry
     WHERE imported_conversation_id = checked_imported_conversation_id;

    IF actual_raw_count <> expected_raw_count
       OR first_raw_position <> 1
       OR last_raw_position <> expected_raw_count
       OR actual_entry_count <> expected_entry_count
       OR first_entry_position <> 1
       OR last_entry_position <> expected_entry_count
       OR EXISTS (
           SELECT 1
             FROM imported_conversation_raw_record AS raw_record
             LEFT JOIN LATERAL (
                 SELECT count(*)::numeric(20, 0) AS actual_count,
                        min(record_entry_position) AS first_position,
                        max(record_entry_position) AS last_position
                   FROM imported_transcript_entry AS entry
                  WHERE entry.imported_conversation_id =
                            raw_record.imported_conversation_id
                    AND entry.raw_record_position =
                            raw_record.raw_record_position
             ) AS membership ON true
            WHERE raw_record.imported_conversation_id =
                      checked_imported_conversation_id
              AND (
                  membership.actual_count <> raw_record.declared_entry_count
                  OR membership.first_position <> 1
                  OR membership.last_position <> raw_record.declared_entry_count
              )
       )
       OR EXISTS (
           SELECT 1
             FROM imported_transcript_entry AS entry
             JOIN (
                 SELECT raw_record_position,
                        COALESCE(
                            sum(declared_entry_count) OVER (
                                ORDER BY raw_record_position
                                ROWS BETWEEN UNBOUNDED PRECEDING
                                    AND 1 PRECEDING
                            ),
                            0
                        ) AS earlier_entry_count
                   FROM imported_conversation_raw_record
                  WHERE imported_conversation_id =
                            checked_imported_conversation_id
             ) AS raw_record_prefix
               ON raw_record_prefix.raw_record_position =
                      entry.raw_record_position
            WHERE entry.imported_conversation_id =
                      checked_imported_conversation_id
              AND entry.imported_entry_position <>
                      raw_record_prefix.earlier_entry_count
                      + entry.record_entry_position
       )
    THEN
        RAISE EXCEPTION
            'imported conversation % does not have complete contiguous membership',
            checked_imported_conversation_id
            USING ERRCODE = '23514';
    END IF;
END;
$$;

CREATE FUNCTION require_imported_conversation_complete()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    PERFORM assert_imported_conversation_complete(
        CASE
            WHEN TG_OP = 'DELETE' THEN OLD.imported_conversation_id
            ELSE NEW.imported_conversation_id
        END
    );
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER imported_conversation_requires_complete_membership
AFTER INSERT OR UPDATE OR DELETE ON imported_conversation
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION require_imported_conversation_complete();

CREATE FUNCTION reject_imported_table_truncate()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION '% cannot be truncated', TG_TABLE_NAME
        USING ERRCODE = '23514';
END;
$$;

CREATE TRIGGER imported_raw_source_record_is_append_only
BEFORE UPDATE OR DELETE ON imported_raw_source_record
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER imported_conversation_is_append_only
BEFORE UPDATE OR DELETE ON imported_conversation
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER imported_conversation_raw_record_is_append_only
BEFORE UPDATE OR DELETE ON imported_conversation_raw_record
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER imported_transcript_entry_is_append_only
BEFORE UPDATE OR DELETE ON imported_transcript_entry
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER imported_raw_source_record_cannot_be_truncated
BEFORE TRUNCATE ON imported_raw_source_record
FOR EACH STATEMENT
EXECUTE FUNCTION reject_imported_table_truncate();

CREATE TRIGGER imported_conversation_cannot_be_truncated
BEFORE TRUNCATE ON imported_conversation
FOR EACH STATEMENT
EXECUTE FUNCTION reject_imported_table_truncate();

CREATE TRIGGER imported_conversation_raw_record_cannot_be_truncated
BEFORE TRUNCATE ON imported_conversation_raw_record
FOR EACH STATEMENT
EXECUTE FUNCTION reject_imported_table_truncate();

CREATE TRIGGER imported_transcript_entry_cannot_be_truncated
BEFORE TRUNCATE ON imported_transcript_entry
FOR EACH STATEMENT
EXECUTE FUNCTION reject_imported_table_truncate();
