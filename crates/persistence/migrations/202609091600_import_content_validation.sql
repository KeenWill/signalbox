CREATE OR REPLACE FUNCTION imported_content_encoding_kind(encoded bytea) RETURNS smallint
    LANGUAGE plpgsql IMMUTABLE STRICT PARALLEL SAFE
    AS $$
DECLARE encoding_version integer; content_kind integer; next_at integer := 3;
BEGIN
    -- Expand compressed input once for every nested byte read.
    encoded := substring(encoded FROM 1);
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
