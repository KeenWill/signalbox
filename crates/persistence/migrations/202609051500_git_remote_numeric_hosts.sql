-- Git push destinations share URL numeric-host admission with the domain parser.
CREATE OR REPLACE FUNCTION configured_git_remote_url_is_valid(candidate text) RETURNS boolean
    LANGUAGE plpgsql IMMUTABLE STRICT PARALLEL SAFE
    AS $$
DECLARE
    host text;
    parts text[];
    part text;
    part_index integer;
    normalized text;
    number bigint;
BEGIN
    IF NOT (
        octet_length(candidate) BETWEEN 9 AND 4096
       AND candidate COLLATE "C" ~ '^[!-~]+$'
       AND candidate COLLATE "C" ~ (
               '^https://'
            || '[A-Za-z0-9._~-]+(:[0-9]{1,5})?'
            || '(/[^?#]*)?$'
           )
       AND coalesce(
               (substring(candidate COLLATE "C"
                          from '^https://[^/?#]*:([0-9]{1,5})(?:[/?#]|$)'))::int,
               1
           ) BETWEEN 1 AND 65535
    ) THEN
        RETURN false;
    END IF;

    host := substring(candidate COLLATE "C" from '^https://([^/:]+)');
    parts := string_to_array(regexp_replace(host, '\.$', ''), '.');
    IF NOT coalesce(parts[cardinality(parts)] COLLATE "C" ~ '^([0-9]+|0[xX][0-9A-Fa-f]*)$', false) THEN
        RETURN true;
    END IF;
    IF cardinality(parts) > 4 THEN
        RETURN false;
    END IF;

    FOR part, part_index IN SELECT value, ordinal::integer
        FROM unnest(parts) WITH ORDINALITY AS component(value, ordinal)
    LOOP
        -- PostgreSQL integer input owns radix decoding; inet does not admit URL shorthand.
        IF part COLLATE "C" ~ '^0[xX][0-9A-Fa-f]*$' THEN
            normalized := '0x' || coalesce(nullif(substring(part from 3), ''), '0');
        ELSIF part COLLATE "C" ~ '^0[0-7]+$' THEN
            normalized := '0o' || part;
        ELSIF part COLLATE "C" ~ '^(0|[1-9][0-9]*)$' THEN
            normalized := part;
        ELSE
            RETURN false;
        END IF;
        IF NOT pg_input_is_valid(normalized, 'bigint') THEN
            RETURN false;
        END IF;
        number := normalized::bigint;
        IF part_index < cardinality(parts) THEN
            IF number > 255 THEN
                RETURN false;
            END IF;
        ELSIF number > (4294967295::bigint >> (8 * (cardinality(parts) - 1))) THEN
            RETURN false;
        END IF;
    END LOOP;
    RETURN true;
END;
$$;
