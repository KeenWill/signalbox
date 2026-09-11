CREATE OR REPLACE FUNCTION first_native_starting_frontier_matches_seed(checked_session uuid, checked_starting_frontier uuid) RETURNS boolean
    LANGUAGE plpgsql STABLE
    AS $$
DECLARE
    checked_ancestry text;
    starting_member_count numeric(20, 0);
    seed_frontier uuid;
    seed_member_count numeric(20, 0);
    actual_seed_member_count bigint;
BEGIN
    SELECT ancestry_kind
      INTO checked_ancestry
      FROM session
     WHERE session_id = checked_session;

    SELECT member_count
      INTO starting_member_count
      FROM context_frontier
     WHERE owning_session_id = checked_session
       AND context_frontier_id = checked_starting_frontier;

    IF checked_ancestry IS NULL OR starting_member_count IS NULL THEN
        RETURN false;
    END IF;

    IF checked_ancestry = 'none' THEN
        RETURN starting_member_count = 1;
    END IF;
    IF checked_ancestry <> 'imported_conversation' THEN
        RETURN false;
    END IF;

    SELECT seed.seed_context_frontier_id, frontier.member_count
      INTO seed_frontier, seed_member_count
      FROM imported_session_seed AS seed
      JOIN context_frontier AS frontier
        ON frontier.owning_session_id = seed.session_id
       AND frontier.context_frontier_id = seed.seed_context_frontier_id
     WHERE seed.session_id = checked_session;

    IF NOT FOUND
       OR seed_member_count IS NULL
       OR starting_member_count IS DISTINCT FROM seed_member_count + 1
    THEN
        RETURN false;
    END IF;

    SELECT count(*)
      INTO actual_seed_member_count
      FROM context_frontier_member
     WHERE owning_session_id = checked_session
       AND context_frontier_id = seed_frontier;
    IF actual_seed_member_count IS DISTINCT FROM seed_member_count THEN
        RETURN false;
    END IF;

    RETURN context_frontier_preserves_prefix(
        checked_session,
        seed_frontier,
        checked_starting_frontier
    );
END;
$$;

DO $search_path_pin$
BEGIN
    EXECUTE format(
        'ALTER FUNCTION first_native_starting_frontier_matches_seed(uuid, uuid) SET search_path TO %I, pg_catalog, pg_temp',
        current_schema()
    );
END
$search_path_pin$;
