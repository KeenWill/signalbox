SET ROLE mod_repo_watch;
SET search_path TO mod_repo_watch, pg_catalog;

UPDATE repository_state
SET comparison_baseline = jsonb_set(
    comparison_baseline,
    '{pull_requests}',
    COALESCE((
        SELECT jsonb_agg(pull || jsonb_build_object('required_check_failure', NULL))
        FROM jsonb_array_elements(comparison_baseline->'pull_requests') AS pull
    ), '[]'::jsonb)
)
WHERE comparison_baseline IS NOT NULL;

DELETE FROM poll_cursor;

RESET ROLE;
RESET search_path;
