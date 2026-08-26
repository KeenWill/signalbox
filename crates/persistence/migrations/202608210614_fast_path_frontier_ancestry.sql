-- Most model-call frontiers extend the turn-start frontier through the
-- immutable prefix chain. Prove that common case from frontier headers alone;
-- preserve the exact member comparison for independently constructed but
-- content-equivalent frontiers.
CREATE OR REPLACE FUNCTION context_frontier_preserves_prefix(
    checked_session_id uuid,
    prefix_frontier_id uuid,
    checked_frontier_id uuid
)
RETURNS boolean
LANGUAGE sql
STABLE
AS $$
    WITH RECURSIVE checked_chain AS MATERIALIZED (
        SELECT
            frontier.context_frontier_id,
            frontier.prefix_context_frontier_id
          FROM context_frontier AS frontier
         WHERE frontier.owning_session_id = checked_session_id
           AND frontier.context_frontier_id = checked_frontier_id
        UNION
        SELECT
            prefix.context_frontier_id,
            prefix.prefix_context_frontier_id
          FROM checked_chain AS chain
          JOIN context_frontier AS prefix
            ON prefix.owning_session_id = checked_session_id
           AND prefix.context_frontier_id =
                   chain.prefix_context_frontier_id
    ),
    prefix_members AS MATERIALIZED (
        SELECT
            member_position,
            source_session_id,
            semantic_entry_id
          FROM context_frontier_member
         WHERE owning_session_id = checked_session_id
           AND context_frontier_id = prefix_frontier_id
    ),
    checked_members AS MATERIALIZED (
        SELECT
            member_position,
            source_session_id,
            semantic_entry_id
          FROM context_frontier_member
         WHERE owning_session_id = checked_session_id
           AND context_frontier_id = checked_frontier_id
    )
    SELECT CASE
        WHEN EXISTS (
            SELECT 1
              FROM checked_chain
             WHERE context_frontier_id = prefix_frontier_id
        ) THEN true
        ELSE NOT EXISTS (
            SELECT 1
              FROM prefix_members AS prefix
              LEFT JOIN checked_members AS checked
                ON checked.member_position = prefix.member_position
             WHERE ROW(
                    checked.source_session_id,
                    checked.semantic_entry_id
               ) IS DISTINCT FROM ROW(
                    prefix.source_session_id,
                    prefix.semantic_entry_id
               )
        )
    END
$$;

DO $migration$
DECLARE
    workflow_schema name := pg_catalog.current_schema();
BEGIN
    IF workflow_schema IS NULL THEN
        RAISE EXCEPTION
            'frontier ancestry fast-path migration requires a current schema';
    END IF;
    EXECUTE pg_catalog.format(
        'ALTER FUNCTION %I.context_frontier_preserves_prefix(uuid,uuid,uuid) '
        'SET search_path TO %I, pg_catalog, pg_temp',
        workflow_schema,
        workflow_schema
    );
END;
$migration$;
