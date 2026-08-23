-- Cursor-compatible support for exact source-session catalog searches. The
-- complete byte equality remains in the query as SHA-256 collision defense.

CREATE INDEX imported_conversation_source_session_catalog_idx
    ON imported_conversation (
        sha256(source_session_id),
        imported_conversation_id
    )
    WHERE source_session_id IS NOT NULL;

-- Descriptor byte totals are immutable snapshot facts. Compute them once for
-- existing imports, and require new imports to persist them with the snapshot.
CREATE TABLE imported_conversation_size_totals (
    imported_conversation_id uuid PRIMARY KEY,
    raw_source_bytes numeric(20, 0) NOT NULL,
    normalized_source_record_bytes numeric(20, 0) NOT NULL,
    normalized_entry_bytes numeric(20, 0) NOT NULL,

    CONSTRAINT imported_conversation_size_totals_nonnegative_u64
        CHECK (
            raw_source_bytes BETWEEN 0 AND 18446744073709551615
            AND normalized_source_record_bytes BETWEEN 0 AND 18446744073709551615
            AND normalized_entry_bytes BETWEEN 0 AND 18446744073709551615
        ),
    CONSTRAINT imported_conversation_size_totals_import_fk
        FOREIGN KEY (imported_conversation_id)
        REFERENCES imported_conversation (imported_conversation_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

INSERT INTO imported_conversation_size_totals (
    imported_conversation_id,
    raw_source_bytes,
    normalized_source_record_bytes,
    normalized_entry_bytes
)
SELECT imported.imported_conversation_id,
       (SELECT COALESCE(SUM(octet_length(blob.raw_bytes)), 0)::numeric
          FROM imported_conversation_raw_record AS occurrence
          JOIN imported_raw_source_record AS blob
            ON blob.content_hash = occurrence.content_hash
         WHERE occurrence.imported_conversation_id = imported.imported_conversation_id),
       (SELECT COALESCE(SUM(octet_length(normalized_value_encoding)), 0)::numeric
          FROM imported_conversation_raw_record AS occurrence
         WHERE occurrence.imported_conversation_id = imported.imported_conversation_id),
       (SELECT COALESCE(SUM(
                    octet_length(content_encoding)
                    + octet_length(source_metadata_encoding)
                ), 0)::numeric
          FROM imported_transcript_entry AS entry
         WHERE entry.imported_conversation_id = imported.imported_conversation_id)
  FROM imported_conversation AS imported;

CREATE TRIGGER imported_conversation_size_totals_is_append_only
BEFORE UPDATE OR DELETE ON imported_conversation_size_totals
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE TRIGGER imported_conversation_size_totals_cannot_be_truncated
BEFORE TRUNCATE ON imported_conversation_size_totals
FOR EACH STATEMENT
EXECUTE FUNCTION reject_imported_table_truncate();

-- Imported entries are immutable. Validate their complete encoding once when
-- this migration backfills existing rows and whenever a new row is inserted,
-- then retain only the compact validated kind needed by bounded discovery. The
-- parser advances over declared payload lengths without copying those payloads.
CREATE FUNCTION imported_encoding_length_at(encoded bytea, start_at integer)
RETURNS bigint LANGUAGE plpgsql IMMUTABLE STRICT PARALLEL SAFE AS $$
DECLARE result bigint := 0; position integer;
BEGIN
    IF start_at < 0 OR start_at + 8 > octet_length(encoded) THEN
        RAISE EXCEPTION 'truncated imported encoding length' USING ERRCODE = '23514';
    END IF;
    FOR position IN start_at..start_at + 7 LOOP
        result := result * 256 + get_byte(encoded, position);
        IF result > 2147483647 THEN
            RAISE EXCEPTION 'imported encoding length is out of range' USING ERRCODE = '23514';
        END IF;
    END LOOP;
    RETURN result;
END; $$;

CREATE FUNCTION imported_encoding_skip_text(encoded bytea, start_at integer)
RETURNS integer LANGUAGE plpgsql IMMUTABLE STRICT PARALLEL SAFE AS $$
DECLARE
    payload_bytes bigint;
    next_at bigint;
    position integer;
    first_byte integer;
    second_byte integer;
BEGIN
    payload_bytes := imported_encoding_length_at(encoded, start_at);
    next_at := start_at::bigint + 8 + payload_bytes;
    IF next_at > octet_length(encoded) THEN
        RAISE EXCEPTION 'truncated imported text encoding' USING ERRCODE = '23514';
    END IF;
    position := start_at + 8;
    WHILE position < next_at LOOP
        first_byte := get_byte(encoded, position);
        IF first_byte <= 127 THEN
            position := position + 1;
        ELSIF first_byte BETWEEN 194 AND 223 THEN
            IF position + 1 >= next_at
                OR get_byte(encoded, position + 1) NOT BETWEEN 128 AND 191 THEN
                RAISE EXCEPTION 'invalid imported text UTF-8' USING ERRCODE = '23514';
            END IF;
            position := position + 2;
        ELSIF first_byte BETWEEN 224 AND 239 THEN
            IF position + 2 >= next_at THEN
                RAISE EXCEPTION 'invalid imported text UTF-8' USING ERRCODE = '23514';
            END IF;
            second_byte := get_byte(encoded, position + 1);
            IF (first_byte = 224 AND second_byte NOT BETWEEN 160 AND 191)
                OR (first_byte = 237 AND second_byte NOT BETWEEN 128 AND 159)
                OR (first_byte NOT IN (224, 237) AND second_byte NOT BETWEEN 128 AND 191)
                OR get_byte(encoded, position + 2) NOT BETWEEN 128 AND 191 THEN
                RAISE EXCEPTION 'invalid imported text UTF-8' USING ERRCODE = '23514';
            END IF;
            position := position + 3;
        ELSIF first_byte BETWEEN 240 AND 244 THEN
            IF position + 3 >= next_at THEN
                RAISE EXCEPTION 'invalid imported text UTF-8' USING ERRCODE = '23514';
            END IF;
            second_byte := get_byte(encoded, position + 1);
            IF (first_byte = 240 AND second_byte NOT BETWEEN 144 AND 191)
                OR (first_byte = 244 AND second_byte NOT BETWEEN 128 AND 143)
                OR (first_byte BETWEEN 241 AND 243 AND second_byte NOT BETWEEN 128 AND 191)
                OR get_byte(encoded, position + 2) NOT BETWEEN 128 AND 191
                OR get_byte(encoded, position + 3) NOT BETWEEN 128 AND 191 THEN
                RAISE EXCEPTION 'invalid imported text UTF-8' USING ERRCODE = '23514';
            END IF;
            position := position + 4;
        ELSE
            RAISE EXCEPTION 'invalid imported text UTF-8' USING ERRCODE = '23514';
        END IF;
    END LOOP;
    RETURN next_at::integer;
END; $$;

CREATE FUNCTION imported_encoding_skip_number(encoded bytea, start_at integer)
RETURNS integer LANGUAGE plpgsql IMMUTABLE STRICT PARALLEL SAFE AS $$
DECLARE payload_bytes bigint; next_at integer; value text;
BEGIN
    payload_bytes := imported_encoding_length_at(encoded, start_at);
    next_at := imported_encoding_skip_text(encoded, start_at);
    value := convert_from(
        substring(encoded FROM start_at + 9 FOR payload_bytes::integer),
        'UTF8'
    );
    IF value !~ '^-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][+-]?[0-9]+)?$' THEN
        RAISE EXCEPTION 'invalid imported JSON number' USING ERRCODE = '23514';
    END IF;
    RETURN next_at;
END; $$;

CREATE FUNCTION imported_encoding_skip_boolean(encoded bytea, start_at integer)
RETURNS integer LANGUAGE plpgsql IMMUTABLE STRICT PARALLEL SAFE AS $$
BEGIN
    IF start_at >= octet_length(encoded) OR get_byte(encoded, start_at) NOT IN (0, 1) THEN
        RAISE EXCEPTION 'invalid imported boolean encoding' USING ERRCODE = '23514';
    END IF;
    RETURN start_at + 1;
END; $$;

CREATE FUNCTION imported_encoding_skip_structured(encoded bytea, start_at integer, nesting_depth integer)
RETURNS integer LANGUAGE plpgsql IMMUTABLE STRICT PARALLEL SAFE AS $$
DECLARE tag integer; item_count bigint; item bigint; next_at integer;
BEGIN
    IF start_at >= octet_length(encoded) THEN
        RAISE EXCEPTION 'invalid imported structured encoding' USING ERRCODE = '23514';
    END IF;
    tag := get_byte(encoded, start_at); next_at := start_at + 1;
    IF tag = 0 THEN RETURN next_at;
    ELSIF tag = 1 THEN RETURN imported_encoding_skip_boolean(encoded, next_at);
    ELSIF tag = 2 THEN RETURN imported_encoding_skip_number(encoded, next_at);
    ELSIF tag = 3 THEN RETURN imported_encoding_skip_text(encoded, next_at);
    ELSIF tag IN (4, 5) THEN
        IF nesting_depth >= 128 THEN
            RAISE EXCEPTION 'imported structured container depth exceeded' USING ERRCODE = '23514';
        END IF;
        item_count := imported_encoding_length_at(encoded, next_at); next_at := next_at + 8;
        IF item_count > octet_length(encoded) - next_at THEN
            RAISE EXCEPTION 'invalid imported structured item count' USING ERRCODE = '23514';
        END IF;
        IF item_count > 0 THEN
            FOR item IN 1..item_count LOOP
                IF tag = 5 THEN next_at := imported_encoding_skip_text(encoded, next_at); END IF;
                next_at := imported_encoding_skip_structured(encoded, next_at, nesting_depth + 1);
            END LOOP;
        END IF;
        RETURN next_at;
    END IF;
    RAISE EXCEPTION 'unsupported imported structured tag %', tag USING ERRCODE = '23514';
END; $$;

CREATE FUNCTION imported_encoding_skip_media_source(encoded bytea, start_at integer)
RETURNS integer LANGUAGE plpgsql IMMUTABLE STRICT PARALLEL SAFE AS $$
DECLARE next_at integer := start_at; field integer; tag integer;
BEGIN
    FOR field IN 1..3 LOOP
        IF next_at >= octet_length(encoded) THEN
            RAISE EXCEPTION 'truncated imported media-source attestation' USING ERRCODE = '23514';
        END IF;
        tag := get_byte(encoded, next_at); next_at := next_at + 1;
        IF tag = 2 THEN next_at := imported_encoding_skip_text(encoded, next_at);
        ELSIF tag NOT IN (0, 1) THEN
            RAISE EXCEPTION 'invalid imported media-source attestation' USING ERRCODE = '23514';
        END IF;
    END LOOP;
    RETURN next_at;
END; $$;

CREATE FUNCTION imported_encoding_skip_attestation(encoded bytea, start_at integer, value_kind text, encoding_version integer)
RETURNS integer LANGUAGE plpgsql IMMUTABLE STRICT PARALLEL SAFE AS $$
DECLARE tag integer; next_at integer;
BEGIN
    IF start_at >= octet_length(encoded) THEN
        RAISE EXCEPTION 'truncated imported source attestation' USING ERRCODE = '23514';
    END IF;
    tag := get_byte(encoded, start_at); next_at := start_at + 1;
    IF tag IN (0, 1) THEN RETURN next_at;
    ELSIF tag <> 2 THEN RAISE EXCEPTION 'invalid imported source attestation' USING ERRCODE = '23514'; END IF;
    IF value_kind = 'text' THEN RETURN imported_encoding_skip_text(encoded, next_at);
    ELSIF value_kind = 'boolean' THEN RETURN imported_encoding_skip_boolean(encoded, next_at);
    ELSIF value_kind = 'structured' THEN RETURN imported_encoding_skip_structured(encoded, next_at, 0);
    ELSIF value_kind = 'media_source' THEN RETURN imported_encoding_skip_media_source(encoded, next_at);
    ELSIF value_kind = 'tool_result' THEN RETURN imported_encoding_skip_tool_result(encoded, next_at, encoding_version);
    END IF;
    RAISE EXCEPTION 'unsupported imported attestation value kind %', value_kind USING ERRCODE = '23514';
END; $$;

CREATE FUNCTION imported_encoding_skip_tool_result(encoded bytea, start_at integer, encoding_version integer)
RETURNS integer LANGUAGE plpgsql IMMUTABLE STRICT PARALLEL SAFE AS $$
DECLARE tag integer; block_count bigint; block bigint; block_tag integer; next_at integer;
BEGIN
    IF start_at >= octet_length(encoded) THEN
        RAISE EXCEPTION 'truncated imported tool-result encoding' USING ERRCODE = '23514';
    END IF;
    tag := get_byte(encoded, start_at); next_at := start_at + 1;
    IF tag = 0 THEN RETURN imported_encoding_skip_text(encoded, next_at);
    ELSIF tag <> 1 THEN RAISE EXCEPTION 'invalid imported tool-result value tag' USING ERRCODE = '23514'; END IF;
    block_count := imported_encoding_length_at(encoded, next_at); next_at := next_at + 8;
    IF block_count > octet_length(encoded) - next_at THEN
        RAISE EXCEPTION 'invalid imported tool-result block count' USING ERRCODE = '23514';
    END IF;
    IF block_count > 0 THEN
        FOR block IN 1..block_count LOOP
            IF next_at >= octet_length(encoded) THEN
                RAISE EXCEPTION 'truncated imported tool-result block' USING ERRCODE = '23514';
            END IF;
            block_tag := get_byte(encoded, next_at); next_at := next_at + 1;
            IF block_tag IN (0, 2) OR (block_tag = 3 AND encoding_version >= 2) THEN
                next_at := imported_encoding_skip_attestation(encoded, next_at, 'text', encoding_version);
            ELSIF block_tag = 1 THEN
                next_at := imported_encoding_skip_attestation(encoded, next_at, 'media_source', encoding_version);
            ELSE RAISE EXCEPTION 'invalid imported tool-result block tag' USING ERRCODE = '23514'; END IF;
        END LOOP;
    END IF;
    RETURN next_at;
END; $$;

CREATE FUNCTION imported_content_encoding_kind(encoded bytea)
RETURNS smallint LANGUAGE plpgsql IMMUTABLE STRICT PARALLEL SAFE AS $$
DECLARE encoding_version integer; content_kind integer; next_at integer := 3;
BEGIN
    IF octet_length(encoded) < 3 THEN RAISE EXCEPTION 'truncated imported content header' USING ERRCODE = '23514'; END IF;
    encoding_version := get_byte(encoded, 0);
    IF encoding_version NOT IN (1, 2) OR get_byte(encoded, 1) <> 1 THEN
        RAISE EXCEPTION 'invalid imported content header' USING ERRCODE = '23514';
    END IF;
    content_kind := get_byte(encoded, 2);
    IF content_kind IN (0, 1, 5, 8) THEN
        next_at := imported_encoding_skip_attestation(encoded, next_at, 'text', encoding_version);
    ELSIF content_kind = 2 THEN
        next_at := imported_encoding_skip_attestation(encoded, next_at, 'text', encoding_version);
        next_at := imported_encoding_skip_attestation(encoded, next_at, 'text', encoding_version);
        next_at := imported_encoding_skip_attestation(encoded, next_at, 'structured', encoding_version);
        next_at := imported_encoding_skip_attestation(encoded, next_at, 'structured', encoding_version);
    ELSIF content_kind = 3 THEN
        next_at := imported_encoding_skip_attestation(encoded, next_at, 'text', encoding_version);
        next_at := imported_encoding_skip_attestation(encoded, next_at, 'tool_result', encoding_version);
        next_at := imported_encoding_skip_attestation(encoded, next_at, 'boolean', encoding_version);
    ELSIF content_kind = 4 THEN
        next_at := imported_encoding_skip_attestation(encoded, next_at, 'text', encoding_version);
        next_at := imported_encoding_skip_attestation(encoded, next_at, 'text', encoding_version);
    ELSIF content_kind = 6 THEN
        next_at := imported_encoding_skip_attestation(encoded, next_at, 'media_source', encoding_version);
    ELSIF content_kind = 7 THEN
        IF next_at >= octet_length(encoded) OR get_byte(encoded, next_at) NOT BETWEEN 0 AND 4 THEN
            RAISE EXCEPTION 'invalid imported message-content absence' USING ERRCODE = '23514';
        END IF; next_at := next_at + 1;
    ELSE RAISE EXCEPTION 'unsupported imported content kind %', content_kind USING ERRCODE = '23514'; END IF;
    IF next_at <> octet_length(encoded) THEN RAISE EXCEPTION 'trailing imported content bytes' USING ERRCODE = '23514'; END IF;
    RETURN content_kind::smallint;
END; $$;

ALTER TABLE imported_transcript_entry
    ADD COLUMN content_kind smallint GENERATED ALWAYS AS (
        imported_content_encoding_kind(content_encoding)
    ) STORED NOT NULL;
