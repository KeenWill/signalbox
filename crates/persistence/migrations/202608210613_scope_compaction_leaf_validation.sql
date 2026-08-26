-- A turn start may reuse only the one leaf of its immutable compaction chain.
-- Keeping leaf selection and the prefix predicate in one inlinable CTE let the
-- planner expand every historical compaction frontier before applying the
-- successor anti-join. Isolate that one candidate first, then reject a leaf
-- shorter than the predecessor from its authoritative header counts.
CREATE OR REPLACE FUNCTION turn_start_effective_predecessor_frontier(
    checked_session uuid,
    checked_predecessor_frontier uuid
)
RETURNS TABLE (
    context_frontier_id uuid,
    member_count numeric(20, 0)
)
LANGUAGE sql
STABLE
AS $$
    WITH leaf AS MATERIALIZED (
        SELECT candidate.result_frontier_id
          FROM context_compaction AS candidate
         WHERE candidate.session_id = checked_session
           AND NOT EXISTS (
                SELECT 1
                  FROM context_compaction AS successor
                 WHERE successor.session_id = candidate.session_id
                   AND successor.predecessor_compaction_id =
                           candidate.context_compaction_id
           )
    ),
    applicable_leaf AS (
        SELECT leaf.result_frontier_id
          FROM leaf
          JOIN context_frontier AS candidate
            ON candidate.owning_session_id = checked_session
           AND candidate.context_frontier_id = leaf.result_frontier_id
          JOIN context_frontier AS predecessor
            ON predecessor.owning_session_id = checked_session
           AND predecessor.context_frontier_id =
                   checked_predecessor_frontier
         WHERE CASE
                   WHEN candidate.member_count < predecessor.member_count
                   THEN false
                   ELSE context_frontier_preserves_prefix(
                        checked_session,
                        checked_predecessor_frontier,
                        leaf.result_frontier_id
                   )
               END
    )
    SELECT frontier.context_frontier_id, frontier.member_count
      FROM context_frontier AS frontier
     WHERE frontier.owning_session_id = checked_session
       AND frontier.context_frontier_id = COALESCE(
            (SELECT result_frontier_id FROM applicable_leaf),
            checked_predecessor_frontier
       )
$$;
