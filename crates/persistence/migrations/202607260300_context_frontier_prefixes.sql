-- Share immutable context-frontier prefixes while preserving every existing
-- frontier identity and exact resolved ordered membership.

DROP TRIGGER context_frontier_is_append_only ON context_frontier;

ALTER TABLE context_frontier
    ADD COLUMN prefix_context_frontier_id uuid;

ALTER TABLE context_frontier
    ADD CONSTRAINT context_frontier_prefix_fk
    FOREIGN KEY (owning_session_id, prefix_context_frontier_id)
    REFERENCES context_frontier (owning_session_id, context_frontier_id)
    ON UPDATE RESTRICT
    ON DELETE RESTRICT
    DEFERRABLE INITIALLY DEFERRED;

-- Existing complete snapshots in one session form an ordered prefix family.
-- Equal-content remints are chained by UUID solely to keep this physical graph
-- acyclic; UUID ordering carries no domain meaning.
UPDATE context_frontier AS child
   SET prefix_context_frontier_id = (
        SELECT parent.context_frontier_id
          FROM context_frontier AS parent
         WHERE parent.owning_session_id = child.owning_session_id
           AND parent.context_frontier_id <> child.context_frontier_id
           AND parent.member_count <= child.member_count
           AND (
                parent.member_count < child.member_count
                OR parent.context_frontier_id < child.context_frontier_id
           )
           AND NOT EXISTS (
                SELECT 1
                  FROM context_frontier_member AS parent_member
                  LEFT JOIN context_frontier_member AS child_member
                    ON child_member.owning_session_id =
                           child.owning_session_id
                   AND child_member.context_frontier_id =
                           child.context_frontier_id
                   AND child_member.member_position =
                           parent_member.member_position
                   AND child_member.source_session_id =
                           parent_member.source_session_id
                   AND child_member.semantic_entry_id =
                           parent_member.semantic_entry_id
                 WHERE parent_member.owning_session_id =
                           parent.owning_session_id
                   AND parent_member.context_frontier_id =
                           parent.context_frontier_id
                   AND child_member.context_frontier_id IS NULL
           )
         ORDER BY parent.member_count DESC,
                  parent.context_frontier_id DESC
         LIMIT 1
   );

CREATE TRIGGER context_frontier_is_append_only
BEFORE UPDATE OR DELETE ON context_frontier
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

ALTER TABLE context_frontier_member
    RENAME TO context_frontier_delta;

DROP TRIGGER context_frontier_member_is_append_only
    ON context_frontier_delta;

DELETE FROM context_frontier_delta AS delta
 USING context_frontier AS frontier,
       context_frontier AS prefix
 WHERE frontier.owning_session_id = delta.owning_session_id
   AND frontier.context_frontier_id = delta.context_frontier_id
   AND prefix.owning_session_id = frontier.owning_session_id
   AND prefix.context_frontier_id = frontier.prefix_context_frontier_id
   AND delta.member_position <= prefix.member_count;

CREATE TRIGGER context_frontier_member_is_append_only
BEFORE UPDATE OR DELETE ON context_frontier_delta
FOR EACH ROW
EXECUTE FUNCTION reject_immutable_record_change();

CREATE FUNCTION resolve_context_frontier_members(
    requested_owning_session_id uuid,
    requested_context_frontier_id uuid
)
RETURNS TABLE (
    owning_session_id uuid,
    context_frontier_id uuid,
    member_position numeric(20, 0),
    source_session_id uuid,
    semantic_entry_id uuid
)
LANGUAGE sql
STABLE
AS $$
    WITH RECURSIVE ancestry (
        owning_session_id,
        context_frontier_id,
        prefix_context_frontier_id
    ) AS (
        SELECT
            frontier.owning_session_id,
            frontier.context_frontier_id,
            frontier.prefix_context_frontier_id
          FROM context_frontier AS frontier
         WHERE frontier.owning_session_id =
                   requested_owning_session_id
           AND frontier.context_frontier_id =
                   requested_context_frontier_id
        UNION
        SELECT
            prefix.owning_session_id,
            prefix.context_frontier_id,
            prefix.prefix_context_frontier_id
          FROM ancestry
          JOIN context_frontier AS prefix
            ON prefix.owning_session_id = ancestry.owning_session_id
           AND prefix.context_frontier_id =
                   ancestry.prefix_context_frontier_id
    )
    SELECT
        requested_owning_session_id,
        requested_context_frontier_id,
        delta.member_position,
        delta.source_session_id,
        delta.semantic_entry_id
      FROM ancestry
      JOIN context_frontier_delta AS delta
        ON delta.owning_session_id = ancestry.owning_session_id
       AND delta.context_frontier_id = ancestry.context_frontier_id
     ORDER BY delta.member_position
$$;

-- Compatibility projection for existing constraint functions. Predicate
-- pushdown first selects the relevant frontier headers; the lateral resolver
-- then walks only each selected prefix chain.
CREATE VIEW context_frontier_member AS
SELECT
    frontier.owning_session_id,
    frontier.context_frontier_id,
    member.member_position,
    member.source_session_id,
    member.semantic_entry_id
  FROM context_frontier AS frontier
 CROSS JOIN LATERAL resolve_context_frontier_members(
    frontier.owning_session_id,
    frontier.context_frontier_id
 ) AS member;

CREATE OR REPLACE FUNCTION reject_context_frontier_member_out_of_bounds()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    declared_member_count numeric(20, 0);
    prefix_member_count numeric(20, 0) := 0;
BEGIN
    SELECT frontier.member_count,
           COALESCE(prefix.member_count, 0)
      INTO declared_member_count, prefix_member_count
      FROM context_frontier AS frontier
      LEFT JOIN context_frontier AS prefix
        ON prefix.owning_session_id = frontier.owning_session_id
       AND prefix.context_frontier_id =
               frontier.prefix_context_frontier_id
     WHERE frontier.owning_session_id = NEW.owning_session_id
       AND frontier.context_frontier_id = NEW.context_frontier_id;

    -- Deltas may precede their deferred-FK header in one transaction. The
    -- header's deferred completeness check validates that ordering.
    IF FOUND
       AND (
           NEW.member_position <= prefix_member_count
           OR NEW.member_position > declared_member_count
       )
    THEN
        RAISE EXCEPTION
            'context frontier delta position % lies outside (%, %]',
            NEW.member_position,
            prefix_member_count,
            declared_member_count
            USING
                ERRCODE = '23514',
                CONSTRAINT = 'context_frontier_member_within_declared_count';
    END IF;

    RETURN NEW;
END;
$$;

CREATE OR REPLACE FUNCTION require_context_frontier_member_within_declared_count()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    declared_member_count numeric(20, 0);
    prefix_member_count numeric(20, 0) := 0;
BEGIN
    SELECT frontier.member_count,
           COALESCE(prefix.member_count, 0)
      INTO declared_member_count, prefix_member_count
      FROM context_frontier AS frontier
      LEFT JOIN context_frontier AS prefix
        ON prefix.owning_session_id = frontier.owning_session_id
       AND prefix.context_frontier_id =
               frontier.prefix_context_frontier_id
     WHERE frontier.owning_session_id = NEW.owning_session_id
       AND frontier.context_frontier_id = NEW.context_frontier_id;

    IF NOT FOUND THEN
        RAISE EXCEPTION
            'context frontier header is unavailable for deferred delta validation'
            USING
                ERRCODE = '23503',
                CONSTRAINT = 'context_frontier_member_requires_visible_header';
    END IF;

    IF NEW.member_position <= prefix_member_count
       OR NEW.member_position > declared_member_count
    THEN
        RAISE EXCEPTION
            'context frontier delta position % lies outside (%, %]',
            NEW.member_position,
            prefix_member_count,
            declared_member_count
            USING
                ERRCODE = '23514',
                CONSTRAINT = 'context_frontier_member_within_declared_count';
    END IF;

    RETURN NULL;
END;
$$;

CREATE OR REPLACE FUNCTION assert_context_frontier_complete_membership(
    checked_owning_session_id uuid,
    checked_context_frontier_id uuid
)
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    expected_count numeric(20, 0);
    actual_count numeric(20, 0);
    distinct_position_count numeric(20, 0);
    distinct_entry_count numeric(20, 0);
    first_position numeric(20, 0);
    last_position numeric(20, 0);
    cycle_found boolean;
BEGIN
    SELECT member_count
      INTO expected_count
      FROM context_frontier
     WHERE owning_session_id = checked_owning_session_id
       AND context_frontier_id = checked_context_frontier_id;

    IF NOT FOUND THEN
        RETURN;
    END IF;

    WITH RECURSIVE prefix_chain (
        context_frontier_id,
        prefix_context_frontier_id,
        visited,
        cycle
    ) AS (
        SELECT
            frontier.context_frontier_id,
            frontier.prefix_context_frontier_id,
            ARRAY[frontier.context_frontier_id],
            false
          FROM context_frontier AS frontier
         WHERE frontier.owning_session_id =
                   checked_owning_session_id
           AND frontier.context_frontier_id =
                   checked_context_frontier_id
        UNION ALL
        SELECT
            prefix.context_frontier_id,
            prefix.prefix_context_frontier_id,
            prefix_chain.visited || prefix.context_frontier_id,
            prefix.context_frontier_id =
                ANY(prefix_chain.visited)
          FROM prefix_chain
          JOIN context_frontier AS prefix
            ON prefix.owning_session_id =
                   checked_owning_session_id
           AND prefix.context_frontier_id =
                   prefix_chain.prefix_context_frontier_id
         WHERE NOT prefix_chain.cycle
    )
    SELECT COALESCE(bool_or(cycle), false)
      INTO cycle_found
      FROM prefix_chain;

    SELECT
        count(*)::numeric(20, 0),
        count(DISTINCT member_position)::numeric(20, 0),
        count(DISTINCT (source_session_id, semantic_entry_id))::numeric(20, 0),
        min(member_position),
        max(member_position)
      INTO
        actual_count,
        distinct_position_count,
        distinct_entry_count,
        first_position,
        last_position
      FROM resolve_context_frontier_members(
        checked_owning_session_id,
        checked_context_frontier_id
      );

    IF cycle_found
       OR actual_count <> expected_count
       OR distinct_position_count <> expected_count
       OR distinct_entry_count <> expected_count
       OR (
           expected_count > 0
           AND (
               first_position <> 1
               OR last_position <> expected_count
           )
       )
    THEN
        RAISE EXCEPTION
            'context frontier (%, %) does not have complete contiguous distinct membership',
            checked_owning_session_id,
            checked_context_frontier_id
            USING ERRCODE = '23514';
    END IF;
END;
$$;
