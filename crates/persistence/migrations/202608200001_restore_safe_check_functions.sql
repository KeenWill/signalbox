-- Functions reachable from check constraints pin their search path.
--
-- pg_restore replays a logical backup with an empty search path and evaluates
-- check constraints while copying table data, so a function body that names
-- another user function without schema qualification resolves during normal
-- operation but fails mid-restore, leaving the backup unrestorable exactly
-- when it is needed. Later function sets (202607310102) already pin their
-- search path at creation; this retrofits the pin onto every check-reachable
-- function the earlier migrations create, plus the one transitive callee
-- (canonical_tool_json names canonical_tool_json_number in its body), so a
-- logical backup restores regardless of how any one body evolves. The pin
-- names the migration-selected schema rather than a literal, preserving
-- installations whose migrations run outside the default schema — the same
-- reason 202607310102 renders its pins through current_schema.
DO $$
DECLARE
    signature text;
BEGIN
    FOREACH signature IN ARRAY ARRAY[
        'canonical_tool_json(text)',
        'canonical_tool_json_number(text)',
        'configured_git_remote_name_is_valid(text)',
        'configured_git_remote_url_is_valid(text)',
        'repo_watch_branch_is_valid(text)',
        'repo_watch_labels_are_valid(text[])',
        'repo_watch_login_is_valid(text)',
        'repo_watch_repository_is_valid(text)',
        'repo_watch_rule_id_is_valid(text)',
        'valid_tool_json(text)',
        'workspace_root_path_is_valid(text)'
    ] LOOP
        EXECUTE format(
            'ALTER FUNCTION %s SET search_path TO %I, pg_catalog, pg_temp',
            signature,
            current_schema
        );
    END LOOP;
END
$$;
