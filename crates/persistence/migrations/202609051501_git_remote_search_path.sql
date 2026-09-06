-- Constraint-reachable functions pin the migration-selected schema for restore.
DO $search_path_pin$
BEGIN
    EXECUTE format(
        'ALTER FUNCTION configured_git_remote_url_is_valid(text) SET search_path TO %I, pg_catalog, pg_temp',
        current_schema()
    );
END
$search_path_pin$;
