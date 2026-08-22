-- Cursor-compatible support for exact source-session catalog searches. The
-- complete byte equality remains in the query as SHA-256 collision defense.

CREATE INDEX imported_conversation_source_session_catalog_idx
    ON imported_conversation (
        sha256(source_session_id),
        imported_conversation_id
    )
    WHERE source_session_id IS NOT NULL;

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
DECLARE payload_bytes bigint; next_at bigint;
BEGIN
    payload_bytes := imported_encoding_length_at(encoded, start_at);
    next_at := start_at::bigint + 8 + payload_bytes;
    IF next_at > octet_length(encoded) THEN
        RAISE EXCEPTION 'truncated imported text encoding' USING ERRCODE = '23514';
    END IF;
    RETURN next_at::integer;
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
    IF nesting_depth >= 128 OR start_at >= octet_length(encoded) THEN
        RAISE EXCEPTION 'invalid imported structured encoding' USING ERRCODE = '23514';
    END IF;
    tag := get_byte(encoded, start_at); next_at := start_at + 1;
    IF tag = 0 THEN RETURN next_at;
    ELSIF tag = 1 THEN RETURN imported_encoding_skip_boolean(encoded, next_at);
    ELSIF tag IN (2, 3) THEN RETURN imported_encoding_skip_text(encoded, next_at);
    ELSIF tag IN (4, 5) THEN
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
