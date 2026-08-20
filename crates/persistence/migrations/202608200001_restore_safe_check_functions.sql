-- Functions reachable from check constraints pin their search path.
--
-- pg_restore replays a logical backup with an empty search_path and evaluates
-- check constraints while copying table data. A plpgsql body that names
-- another user function without schema qualification therefore resolves during
-- normal operation but fails mid-restore: the first restore rehearsal against
-- a live backup stopped at tool_request's data copy because
-- canonical_tool_json calls canonical_tool_json_number unqualified. Later
-- function sets (202607310102) already carry the pin; this retrofits every
-- function the current schema reaches from a check constraint, plus that one
-- transitive callee, so the set stays restore-safe regardless of how any one
-- body evolves.

ALTER FUNCTION canonical_tool_json(text)
    SET search_path = public, pg_catalog, pg_temp;

ALTER FUNCTION canonical_tool_json_number(text)
    SET search_path = public, pg_catalog, pg_temp;

ALTER FUNCTION configured_git_remote_name_is_valid(text)
    SET search_path = public, pg_catalog, pg_temp;

ALTER FUNCTION configured_git_remote_url_is_valid(text)
    SET search_path = public, pg_catalog, pg_temp;

ALTER FUNCTION repo_watch_branch_is_valid(text)
    SET search_path = public, pg_catalog, pg_temp;

ALTER FUNCTION repo_watch_convergence_check_names_are_valid(text[])
    SET search_path = public, pg_catalog, pg_temp;

ALTER FUNCTION repo_watch_convergence_threads_are_valid(text[])
    SET search_path = public, pg_catalog, pg_temp;

ALTER FUNCTION repo_watch_labels_are_valid(text[])
    SET search_path = public, pg_catalog, pg_temp;

ALTER FUNCTION repo_watch_login_is_valid(text)
    SET search_path = public, pg_catalog, pg_temp;

ALTER FUNCTION repo_watch_repository_is_valid(text)
    SET search_path = public, pg_catalog, pg_temp;

ALTER FUNCTION repo_watch_rule_id_is_valid(text)
    SET search_path = public, pg_catalog, pg_temp;

ALTER FUNCTION valid_tool_json(text)
    SET search_path = public, pg_catalog, pg_temp;

ALTER FUNCTION workspace_root_path_is_valid(text)
    SET search_path = public, pg_catalog, pg_temp;
