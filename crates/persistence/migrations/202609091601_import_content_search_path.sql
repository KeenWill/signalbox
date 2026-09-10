DO $search_path_pin$
BEGIN
    EXECUTE format(
        'ALTER FUNCTION imported_content_encoding_kind(bytea) SET search_path TO %I, pg_catalog, pg_temp',
        current_schema()
    );
END
$search_path_pin$;
