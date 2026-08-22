-- Bind each user-visible runner relocation to its exact successor placement
-- and to the frontier that installs the semantic boundary.

ALTER TABLE semantic_transcript_entry
    ADD COLUMN runner_placement_revision numeric(20, 0),
    ADD COLUMN runner_placement_event_ordinal numeric(20, 0);

DO $migration$
DECLARE
    legacy_kind text;
    legacy_shape text;
    legacy_payload_nulls text;
BEGIN
    SELECT pg_get_expr(record.conbin, record.conrelid)
      INTO legacy_kind
      FROM pg_constraint AS record
     WHERE record.conrelid = 'semantic_transcript_entry'::regclass
       AND record.conname = 'semantic_transcript_entry_payload_kind_closed';
    SELECT pg_get_expr(record.conbin, record.conrelid)
      INTO legacy_shape
      FROM pg_constraint AS record
     WHERE record.conrelid = 'semantic_transcript_entry'::regclass
       AND record.conname = 'semantic_transcript_entry_payload_shape';
    SELECT string_agg(format('%I IS NULL', attribute.attname), ' AND ')
      INTO legacy_payload_nulls
      FROM pg_attribute AS attribute
     WHERE attribute.attrelid = 'semantic_transcript_entry'::regclass
       AND attribute.attnum > 0
       AND NOT attribute.attisdropped
       AND attribute.attname NOT IN (
            'source_session_id', 'semantic_entry_id', 'payload_kind',
            'runner_placement_revision', 'runner_placement_event_ordinal'
       );
    IF legacy_kind IS NULL OR legacy_shape IS NULL
        OR legacy_payload_nulls IS NULL THEN
        RAISE EXCEPTION
            'semantic transcript legacy runner-placement shape is missing';
    END IF;

    ALTER TABLE semantic_transcript_entry
        DROP CONSTRAINT semantic_transcript_entry_payload_kind_closed,
        DROP CONSTRAINT semantic_transcript_entry_payload_shape;
    EXECUTE format(
        'ALTER TABLE semantic_transcript_entry
             ADD CONSTRAINT semantic_transcript_entry_payload_kind_closed
                 CHECK (payload_kind = ''runner_placement_changed'' OR (%s)),
             ADD CONSTRAINT semantic_transcript_entry_payload_shape CHECK (
                 (payload_kind = ''runner_placement_changed''
                    AND runner_placement_revision IS NOT NULL
                    AND runner_placement_event_ordinal IS NOT NULL
                    AND %s)
                 OR (payload_kind <> ''runner_placement_changed''
                    AND runner_placement_revision IS NULL
                    AND runner_placement_event_ordinal IS NULL
                    AND (%s))
             )',
        legacy_kind,
        legacy_payload_nulls,
        legacy_shape
    );
END;
$migration$;

ALTER TABLE semantic_transcript_entry
    ADD CONSTRAINT semantic_transcript_entry_runner_placement_positive_u64
        CHECK (
            runner_placement_revision IS NULL
            OR (
                runner_placement_revision
                    BETWEEN 1 AND 18446744073709551615
                AND runner_placement_event_ordinal
                    BETWEEN 1 AND 18446744073709551615
            )
        ),
    ADD CONSTRAINT semantic_transcript_entry_runner_placement_once
        UNIQUE (source_session_id, runner_placement_revision),
    ADD CONSTRAINT semantic_transcript_entry_runner_placement_reference_key
        UNIQUE (
            source_session_id,
            semantic_entry_id,
            runner_placement_revision
        ),
    ADD CONSTRAINT semantic_transcript_entry_runner_placement_fk
        FOREIGN KEY (
            source_session_id,
            runner_placement_event_ordinal,
            runner_placement_revision
        )
        REFERENCES runner_session_placement_record (
            session_id,
            event_ordinal,
            placement_revision
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED;

-- Supersedes the generic turn-authority trigger predicates from
-- 202608020018_session_delegation.sql. Runner placement entries are instead
-- authorized by the exact successor-placement and frontier relations below.
DROP TRIGGER semantic_entry_requires_matching_turn_state
    ON semantic_transcript_entry;
DROP TRIGGER semantic_entry_update_requires_matching_turn_state
    ON semantic_transcript_entry;
DROP TRIGGER semantic_entry_delete_requires_matching_turn_state
    ON semantic_transcript_entry;
CREATE CONSTRAINT TRIGGER semantic_entry_requires_matching_turn_state
AFTER INSERT ON semantic_transcript_entry
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW WHEN (
    NEW.payload_kind NOT IN (
        'delegated_task', 'delegation_message', 'delegation_result',
        'runner_placement_changed'
    )
)
EXECUTE FUNCTION require_semantic_entry_turn_state();
CREATE CONSTRAINT TRIGGER semantic_entry_update_requires_matching_turn_state
AFTER UPDATE ON semantic_transcript_entry
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW WHEN (
    OLD.payload_kind NOT IN (
        'delegated_task', 'delegation_message', 'delegation_result',
        'runner_placement_changed'
    )
    OR NEW.payload_kind NOT IN (
        'delegated_task', 'delegation_message', 'delegation_result',
        'runner_placement_changed'
    )
)
EXECUTE FUNCTION require_semantic_entry_turn_state();
CREATE CONSTRAINT TRIGGER semantic_entry_delete_requires_matching_turn_state
AFTER DELETE ON semantic_transcript_entry
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW WHEN (
    OLD.payload_kind NOT IN (
        'delegated_task', 'delegation_message', 'delegation_result',
        'runner_placement_changed'
    )
)
EXECUTE FUNCTION require_semantic_entry_turn_state();

CREATE TABLE session_runner_placement_frontier (
    session_id uuid NOT NULL,
    placement_revision numeric(20, 0) NOT NULL,
    semantic_entry_id uuid NOT NULL,
    context_frontier_id uuid NOT NULL,

    CONSTRAINT session_runner_placement_frontier_pk
        PRIMARY KEY (session_id, placement_revision),
    CONSTRAINT session_runner_placement_frontier_revision_positive_u64
        CHECK (
            placement_revision BETWEEN 1 AND 18446744073709551615
        ),
    CONSTRAINT session_runner_placement_frontier_entry_once
        UNIQUE (session_id, semantic_entry_id),
    CONSTRAINT session_runner_placement_frontier_snapshot_once
        UNIQUE (session_id, context_frontier_id),
    CONSTRAINT session_runner_placement_frontier_entry_fk
        FOREIGN KEY (
            session_id,
            semantic_entry_id,
            placement_revision
        )
        REFERENCES semantic_transcript_entry (
            source_session_id,
            semantic_entry_id,
            runner_placement_revision
        )
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT session_runner_placement_frontier_snapshot_fk
        FOREIGN KEY (session_id, context_frontier_id)
        REFERENCES context_frontier (owning_session_id, context_frontier_id)
        ON UPDATE RESTRICT
        ON DELETE RESTRICT
        DEFERRABLE INITIALLY DEFERRED
);

CREATE TRIGGER session_runner_placement_frontier_is_append_only
BEFORE UPDATE OR DELETE ON session_runner_placement_frontier
FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();
CREATE TRIGGER session_runner_placement_frontier_rejects_truncate
BEFORE TRUNCATE ON session_runner_placement_frontier
FOR EACH STATEMENT EXECUTE FUNCTION reject_immutable_record_change();

-- Placement boundaries are out-of-band while a yielded tool round is being
-- completed. Match the first proposal-count non-placement entries after the
-- predecessor boundary as ordered results, allowing placement entries before
-- or between them while still rejecting every other interruption.
CREATE OR REPLACE FUNCTION continuation_frontier_closes_predecessor_tool_round(
    checked_attempt_id uuid,
    checked_turn_id uuid,
    checked_session_id uuid,
    checked_frontier_id uuid
)
RETURNS boolean
LANGUAGE sql
STABLE
AS $$
    SELECT EXISTS (
        SELECT 1
          FROM turn_attempt AS continuation_attempt
          JOIN model_call AS predecessor_call
            ON predecessor_call.turn_attempt_id =
               continuation_attempt.continued_from_attempt_id
           AND predecessor_call.turn_id = continuation_attempt.turn_id
           AND predecessor_call.session_id = continuation_attempt.session_id
           AND predecessor_call.state_kind = 'terminal'
           AND predecessor_call.terminal_disposition_kind = 'completed'
          JOIN tool_round AS predecessor_round
            ON predecessor_round.producing_model_call_id =
               predecessor_call.model_call_id
           AND predecessor_round.turn_id = predecessor_call.turn_id
           AND predecessor_round.session_id = predecessor_call.session_id
           AND predecessor_round.boundary_kind = 'continuing'
          JOIN context_frontier AS boundary
            ON boundary.owning_session_id = predecessor_round.session_id
           AND boundary.context_frontier_id =
               predecessor_round.boundary_frontier_id
         WHERE continuation_attempt.turn_attempt_id = checked_attempt_id
           AND continuation_attempt.turn_id = checked_turn_id
           AND continuation_attempt.session_id = checked_session_id
           AND continuation_attempt.continued_from_attempt_id IS NOT NULL
           AND NOT EXISTS (
                SELECT 1
                  FROM context_frontier_member AS boundary_member
                  LEFT JOIN context_frontier_member AS checked_member
                    ON checked_member.owning_session_id =
                       boundary_member.owning_session_id
                   AND checked_member.context_frontier_id =
                       checked_frontier_id
                   AND checked_member.member_position =
                       boundary_member.member_position
                   AND checked_member.source_session_id =
                       boundary_member.source_session_id
                   AND checked_member.semantic_entry_id =
                       boundary_member.semantic_entry_id
                 WHERE boundary_member.owning_session_id =
                       predecessor_round.session_id
                   AND boundary_member.context_frontier_id =
                       predecessor_round.boundary_frontier_id
                   AND checked_member.member_position IS NULL
           )
           AND NOT EXISTS (
                SELECT 1
                  FROM generate_series(
                        0,
                        predecessor_round.request_count::bigint - 1
                  ) AS expected(request_ordinal)
                  JOIN tool_request AS request
                    ON request.producing_model_call_id =
                       predecessor_round.producing_model_call_id
                   AND request.request_ordinal =
                       expected.request_ordinal
                  LEFT JOIN LATERAL (
                        SELECT result_entry.semantic_entry_id,
                               result_entry.payload_kind,
                               result_entry.tool_result_request_id,
                               result_attempt.request_id AS attempt_request_id
                          FROM context_frontier_member AS result_member
                          JOIN semantic_transcript_entry AS result_entry
                            ON result_entry.source_session_id =
                               result_member.source_session_id
                           AND result_entry.semantic_entry_id =
                               result_member.semantic_entry_id
                          LEFT JOIN tool_attempt AS result_attempt
                            ON result_attempt.attempt_id =
                               result_entry.tool_result_attempt_id
                         WHERE result_member.owning_session_id =
                               predecessor_round.session_id
                           AND result_member.context_frontier_id =
                               checked_frontier_id
                           AND result_member.member_position >
                               boundary.member_count
                           AND result_entry.payload_kind <>
                               'runner_placement_changed'
                         ORDER BY result_member.member_position
                         OFFSET expected.request_ordinal
                         LIMIT 1
                  ) AS result ON true
                 WHERE result.semantic_entry_id IS NULL
                    OR result.payload_kind NOT IN (
                        'tool_execution_result',
                        'tool_denied',
                        'tool_closed_by_turn_end'
                    )
                    OR (
                        result.tool_result_request_id
                            IS DISTINCT FROM request.request_id
                        AND result.attempt_request_id
                            IS DISTINCT FROM request.request_id
                    )
           )
           AND (
                SELECT count(*)
                  FROM context_frontier_member AS suffix_member
                  JOIN semantic_transcript_entry AS suffix_entry
                    ON suffix_entry.source_session_id =
                       suffix_member.source_session_id
                   AND suffix_entry.semantic_entry_id =
                       suffix_member.semantic_entry_id
                 WHERE suffix_member.owning_session_id =
                       predecessor_round.session_id
                   AND suffix_member.context_frontier_id =
                       checked_frontier_id
                   AND suffix_member.member_position > boundary.member_count
                   AND suffix_entry.payload_kind <>
                       'runner_placement_changed'
           ) = predecessor_round.request_count
    );
$$;

-- Runner-recovery cancellation retains the yielded frontier as correlation
-- identity, but result projection may start from a later compatible placement
-- frontier. Use that effective projection-base count before validating the
-- proposal-ordered result suffix and terminal cancellation entry.
DO $migration$
DECLARE
    definition text;
    updated_definition text;
    old_member_count CONSTANT text := $old$
    SELECT member_count
      INTO base_member_count
      FROM context_frontier
     WHERE owning_session_id = checked_session
       AND context_frontier_id = base_frontier;
$old$;
    new_member_count CONSTANT text := $new$
    IF runner_recovery_effect.command_id IS NOT NULL THEN
        SELECT max(candidate.member_count)
          INTO base_member_count
          FROM (
                SELECT frontier.context_frontier_id, frontier.member_count
                  FROM context_frontier AS frontier
                 WHERE frontier.owning_session_id = checked_session
                   AND frontier.context_frontier_id = base_frontier
                UNION ALL
                SELECT frontier.context_frontier_id, frontier.member_count
                  FROM session_runner_placement_frontier AS pointer
                  JOIN context_frontier AS frontier
                    ON frontier.owning_session_id = pointer.session_id
                   AND frontier.context_frontier_id =
                           pointer.context_frontier_id
                 WHERE pointer.session_id = checked_session
          ) AS candidate
         WHERE NOT EXISTS (
                SELECT 1
                  FROM context_frontier_member AS candidate_member
                  LEFT JOIN context_frontier_member AS terminal_member
                    ON terminal_member.owning_session_id = checked_session
                   AND terminal_member.context_frontier_id =
                           checked_terminal_frontier
                   AND terminal_member.member_position =
                           candidate_member.member_position
                   AND terminal_member.source_session_id =
                           candidate_member.source_session_id
                   AND terminal_member.semantic_entry_id =
                           candidate_member.semantic_entry_id
                 WHERE candidate_member.owning_session_id = checked_session
                   AND candidate_member.context_frontier_id =
                           candidate.context_frontier_id
                   AND terminal_member.member_position IS NULL
         )
           AND NOT EXISTS (
                SELECT 1
                  FROM context_frontier_member AS yielded_member
                  LEFT JOIN context_frontier_member AS candidate_member
                    ON candidate_member.owning_session_id = checked_session
                   AND candidate_member.context_frontier_id =
                           candidate.context_frontier_id
                   AND candidate_member.member_position =
                           yielded_member.member_position
                   AND candidate_member.source_session_id =
                           yielded_member.source_session_id
                   AND candidate_member.semantic_entry_id =
                           yielded_member.semantic_entry_id
                 WHERE yielded_member.owning_session_id = checked_session
                   AND yielded_member.context_frontier_id = base_frontier
                   AND candidate_member.member_position IS NULL
         );
    ELSE
        SELECT member_count
          INTO base_member_count
          FROM context_frontier
         WHERE owning_session_id = checked_session
           AND context_frontier_id = base_frontier;
    END IF;
$new$;
BEGIN
    SELECT pg_get_functiondef(
        'assert_cancelled_turn_final_state(uuid)'::regprocedure
    )
      INTO definition;
    IF (
        length(definition) -
        length(replace(definition, old_member_count, ''))
    ) / length(old_member_count) <> 1
    THEN
        RAISE EXCEPTION
            'unexpected cancelled-turn projection-base validator';
    END IF;
    updated_definition := replace(
        definition, old_member_count, new_member_count
    );
    EXECUTE updated_definition;
END;
$migration$;

-- Turn activation treats the latest relocation boundary as part of the
-- authoritative predecessor prefix. The helper keeps the existing imported
-- seed and predecessor rules while admitting that one exact intervening
-- boundary before the fresh origin entry.
CREATE FUNCTION turn_starting_frontier_extends_current_base(
    checked_session_id uuid,
    checked_starting_frontier_id uuid,
    ordinary_base_frontier_id uuid
)
RETURNS boolean LANGUAGE plpgsql STABLE AS $function$
DECLARE
    placement_base_frontier_id uuid;
    effective_base_frontier_id uuid;
    starting_member_count numeric(20, 0);
    ordinary_member_count numeric(20, 0);
    placement_member_count numeric(20, 0);
    effective_member_count numeric(20, 0);
    missing_member_count bigint;
BEGIN
    SELECT frontier.member_count
      INTO starting_member_count
      FROM context_frontier AS frontier
     WHERE frontier.owning_session_id = checked_session_id
       AND frontier.context_frontier_id = checked_starting_frontier_id;
    IF starting_member_count IS NULL THEN
        RETURN false;
    END IF;

    SELECT pointer.context_frontier_id
      INTO placement_base_frontier_id
      FROM runner_current_session_placement AS head
      JOIN runner_session_placement_record AS placement
        ON placement.session_id = head.session_id
       AND placement.event_ordinal = head.event_ordinal
      JOIN session_runner_placement_frontier AS pointer
        ON pointer.session_id = placement.session_id
       AND pointer.placement_revision = placement.placement_revision
     WHERE head.session_id = checked_session_id;

    IF ordinary_base_frontier_id IS NULL
       AND placement_base_frontier_id IS NULL
    THEN
        RETURN starting_member_count = 1;
    END IF;
    IF ordinary_base_frontier_id IS NULL THEN
        effective_base_frontier_id := placement_base_frontier_id;
    ELSIF placement_base_frontier_id IS NULL THEN
        effective_base_frontier_id := ordinary_base_frontier_id;
    ELSE
        SELECT member_count
          INTO ordinary_member_count
          FROM context_frontier
         WHERE owning_session_id = checked_session_id
           AND context_frontier_id = ordinary_base_frontier_id;
        SELECT member_count
          INTO placement_member_count
          FROM context_frontier
         WHERE owning_session_id = checked_session_id
           AND context_frontier_id = placement_base_frontier_id;
        IF ordinary_member_count IS NULL OR placement_member_count IS NULL THEN
            RETURN false;
        END IF;
        IF ordinary_member_count <= placement_member_count
           AND NOT EXISTS (
                SELECT 1
                  FROM context_frontier_member AS ordinary_member
                  LEFT JOIN context_frontier_member AS placement_member
                    ON placement_member.owning_session_id = checked_session_id
                   AND placement_member.context_frontier_id =
                           placement_base_frontier_id
                   AND placement_member.member_position =
                           ordinary_member.member_position
                   AND placement_member.source_session_id =
                           ordinary_member.source_session_id
                   AND placement_member.semantic_entry_id =
                           ordinary_member.semantic_entry_id
                 WHERE ordinary_member.owning_session_id = checked_session_id
                   AND ordinary_member.context_frontier_id =
                           ordinary_base_frontier_id
                   AND placement_member.member_position IS NULL
           )
        THEN
            effective_base_frontier_id := placement_base_frontier_id;
        ELSIF placement_member_count <= ordinary_member_count
           AND NOT EXISTS (
                SELECT 1
                  FROM context_frontier_member AS placement_member
                  LEFT JOIN context_frontier_member AS ordinary_member
                    ON ordinary_member.owning_session_id = checked_session_id
                   AND ordinary_member.context_frontier_id =
                           ordinary_base_frontier_id
                   AND ordinary_member.member_position =
                           placement_member.member_position
                   AND ordinary_member.source_session_id =
                           placement_member.source_session_id
                   AND ordinary_member.semantic_entry_id =
                           placement_member.semantic_entry_id
                 WHERE placement_member.owning_session_id = checked_session_id
                   AND placement_member.context_frontier_id =
                           placement_base_frontier_id
                   AND ordinary_member.member_position IS NULL
           )
        THEN
            effective_base_frontier_id := ordinary_base_frontier_id;
        ELSE
            RETURN false;
        END IF;
    END IF;

    SELECT member_count
      INTO effective_member_count
      FROM context_frontier
     WHERE owning_session_id = checked_session_id
       AND context_frontier_id = effective_base_frontier_id;
    SELECT count(*)
      INTO missing_member_count
      FROM context_frontier_member AS base_member
      LEFT JOIN context_frontier_member AS starting_member
        ON starting_member.owning_session_id = checked_session_id
       AND starting_member.context_frontier_id = checked_starting_frontier_id
       AND starting_member.member_position = base_member.member_position
       AND starting_member.source_session_id = base_member.source_session_id
       AND starting_member.semantic_entry_id = base_member.semantic_entry_id
     WHERE base_member.owning_session_id = checked_session_id
       AND base_member.context_frontier_id = effective_base_frontier_id
       AND starting_member.member_position IS NULL;

    RETURN effective_member_count IS NOT NULL
       AND starting_member_count = effective_member_count + 1
       AND missing_member_count = 0;
END;
$function$;

CREATE OR REPLACE FUNCTION first_native_starting_frontier_matches_seed(
    checked_session uuid,
    checked_starting_frontier uuid
)
RETURNS boolean LANGUAGE plpgsql STABLE AS $function$
DECLARE
    checked_ancestry text;
    seed_frontier uuid;
BEGIN
    SELECT ancestry_kind
      INTO checked_ancestry
      FROM session
     WHERE session_id = checked_session;
    IF checked_ancestry = 'none' THEN
        RETURN turn_starting_frontier_extends_current_base(
            checked_session,
            checked_starting_frontier,
            NULL
        );
    END IF;
    IF checked_ancestry <> 'imported_conversation' THEN
        RETURN false;
    END IF;
    SELECT seed_context_frontier_id
      INTO seed_frontier
      FROM imported_session_seed
     WHERE session_id = checked_session;
    IF NOT FOUND THEN
        RETURN false;
    END IF;
    RETURN turn_starting_frontier_extends_current_base(
        checked_session,
        checked_starting_frontier,
        seed_frontier
    );
END;
$function$;

CREATE OR REPLACE FUNCTION turn_start_effective_predecessor_frontier(
    checked_session uuid,
    checked_predecessor_frontier uuid
)
RETURNS TABLE (
    context_frontier_id uuid,
    member_count numeric(20, 0)
)
LANGUAGE sql STABLE AS $function$
    WITH applicable_leaf AS (
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
           AND NOT EXISTS (
                SELECT 1
                  FROM context_frontier_member AS predecessor_member
                  LEFT JOIN context_frontier_member AS result_member
                    ON result_member.owning_session_id = checked_session
                   AND result_member.context_frontier_id =
                           candidate.result_frontier_id
                   AND result_member.member_position =
                           predecessor_member.member_position
                   AND result_member.source_session_id =
                           predecessor_member.source_session_id
                   AND result_member.semantic_entry_id =
                           predecessor_member.semantic_entry_id
                 WHERE predecessor_member.owning_session_id = checked_session
                   AND predecessor_member.context_frontier_id =
                           checked_predecessor_frontier
                   AND result_member.member_position IS NULL
           )
    ),
    ordinary_base AS (
        SELECT frontier.context_frontier_id, frontier.member_count
          FROM context_frontier AS frontier
         WHERE frontier.owning_session_id = checked_session
           AND frontier.context_frontier_id = COALESCE(
                (SELECT result_frontier_id FROM applicable_leaf),
                checked_predecessor_frontier
           )
    ),
    placement_base AS (
        SELECT frontier.context_frontier_id, frontier.member_count
          FROM runner_current_session_placement AS head
          JOIN runner_session_placement_record AS placement
            ON placement.session_id = head.session_id
           AND placement.event_ordinal = head.event_ordinal
          JOIN session_runner_placement_frontier AS pointer
            ON pointer.session_id = placement.session_id
           AND pointer.placement_revision = placement.placement_revision
          JOIN context_frontier AS frontier
            ON frontier.owning_session_id = pointer.session_id
           AND frontier.context_frontier_id = pointer.context_frontier_id
         WHERE head.session_id = checked_session
    ),
    candidate AS (
        SELECT placement.context_frontier_id, placement.member_count
          FROM ordinary_base AS ordinary
          JOIN placement_base AS placement ON true
         WHERE ordinary.member_count <= placement.member_count
           AND NOT EXISTS (
                SELECT 1
                  FROM context_frontier_member AS ordinary_member
                  LEFT JOIN context_frontier_member AS placement_member
                    ON placement_member.owning_session_id = checked_session
                   AND placement_member.context_frontier_id =
                           placement.context_frontier_id
                   AND placement_member.member_position =
                           ordinary_member.member_position
                   AND placement_member.source_session_id =
                           ordinary_member.source_session_id
                   AND placement_member.semantic_entry_id =
                           ordinary_member.semantic_entry_id
                 WHERE ordinary_member.owning_session_id = checked_session
                   AND ordinary_member.context_frontier_id =
                           ordinary.context_frontier_id
                   AND placement_member.member_position IS NULL
           )
        UNION ALL
        SELECT ordinary.context_frontier_id, ordinary.member_count
          FROM ordinary_base AS ordinary
          LEFT JOIN placement_base AS placement ON true
         WHERE placement.context_frontier_id IS NULL
            OR (
                placement.member_count <= ordinary.member_count
                AND NOT EXISTS (
                    SELECT 1
                      FROM context_frontier_member AS placement_member
                      LEFT JOIN context_frontier_member AS ordinary_member
                        ON ordinary_member.owning_session_id = checked_session
                       AND ordinary_member.context_frontier_id =
                               ordinary.context_frontier_id
                       AND ordinary_member.member_position =
                               placement_member.member_position
                       AND ordinary_member.source_session_id =
                               placement_member.source_session_id
                       AND ordinary_member.semantic_entry_id =
                               placement_member.semantic_entry_id
                     WHERE placement_member.owning_session_id = checked_session
                       AND placement_member.context_frontier_id =
                               placement.context_frontier_id
                       AND ordinary_member.member_position IS NULL
                )
            )
    )
    SELECT candidate.context_frontier_id, candidate.member_count
      FROM candidate
     ORDER BY candidate.member_count DESC
     LIMIT 1
$function$;

CREATE FUNCTION require_runner_placement_frontier_boundary(
    checked_session_id uuid,
    checked_placement_revision numeric(20, 0)
)
RETURNS void LANGUAGE plpgsql AS $function$
DECLARE
    matching_boundaries bigint;
BEGIN
    SELECT count(*)
      INTO matching_boundaries
      FROM session_runner_placement_frontier AS pointer
      JOIN semantic_transcript_entry AS entry
        ON entry.source_session_id = pointer.session_id
       AND entry.semantic_entry_id = pointer.semantic_entry_id
       AND entry.runner_placement_revision = pointer.placement_revision
       AND entry.payload_kind = 'runner_placement_changed'
      JOIN runner_session_placement_record AS placement
        ON placement.session_id = entry.source_session_id
       AND placement.event_ordinal = entry.runner_placement_event_ordinal
       AND placement.placement_revision = entry.runner_placement_revision
       AND placement.event_kind IN ('runner_replaced', 'profile_replaced')
       AND placement.state_kind = 'pinned'
      JOIN runner_current_session_placement AS current_placement
        ON current_placement.session_id = placement.session_id
       AND current_placement.event_ordinal = placement.event_ordinal
      JOIN context_frontier AS frontier
        ON frontier.owning_session_id = pointer.session_id
       AND frontier.context_frontier_id = pointer.context_frontier_id
       AND frontier.member_count >= 1
      LEFT JOIN context_frontier AS prefix
        ON prefix.owning_session_id = frontier.owning_session_id
       AND prefix.context_frontier_id = frontier.prefix_context_frontier_id
      JOIN context_frontier_member AS member
        ON member.owning_session_id = frontier.owning_session_id
       AND member.context_frontier_id = frontier.context_frontier_id
       AND member.member_position = frontier.member_count
       AND member.source_session_id = entry.source_session_id
       AND member.semantic_entry_id = entry.semantic_entry_id
     WHERE pointer.session_id = checked_session_id
       AND pointer.placement_revision = checked_placement_revision
       AND (
            (
                frontier.prefix_context_frontier_id IS NULL
                AND frontier.member_count = 1
                AND NOT EXISTS (
                    SELECT 1
                      FROM context_frontier AS prior_frontier
                     WHERE prior_frontier.owning_session_id = pointer.session_id
                       AND prior_frontier.context_frontier_id <>
                               frontier.context_frontier_id
                       AND prior_frontier.member_count > 0
                )
            )
            OR (
                prefix.context_frontier_id IS NOT NULL
                AND frontier.member_count = prefix.member_count + 1
                AND NOT EXISTS (
                    SELECT 1
                      FROM context_frontier AS existing_successor
                     WHERE existing_successor.owning_session_id =
                               prefix.owning_session_id
                       AND existing_successor.prefix_context_frontier_id =
                               prefix.context_frontier_id
                       AND existing_successor.context_frontier_id <>
                               frontier.context_frontier_id
                )
            )
       );

    IF matching_boundaries <> 1 THEN
        RAISE EXCEPTION
            'runner placement frontier requires one exact prefix-extending successor boundary'
            USING ERRCODE = '23514',
                CONSTRAINT = 'runner_placement_frontier_boundary_required';
    END IF;
END;
$function$;

CREATE FUNCTION recheck_runner_placement_frontier_boundary()
RETURNS trigger LANGUAGE plpgsql AS $function$
BEGIN
    PERFORM require_runner_placement_frontier_boundary(
        NEW.session_id,
        NEW.placement_revision
    );
    RETURN NULL;
END;
$function$;

CREATE CONSTRAINT TRIGGER runner_placement_frontier_boundary_is_checked
AFTER INSERT ON session_runner_placement_frontier
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION recheck_runner_placement_frontier_boundary();

CREATE FUNCTION require_runner_placement_semantic_frontier()
RETURNS trigger LANGUAGE plpgsql AS $function$
BEGIN
    IF NEW.payload_kind = 'runner_placement_changed' THEN
        PERFORM require_runner_placement_frontier_boundary(
            NEW.source_session_id,
            NEW.runner_placement_revision
        );
    END IF;
    RETURN NULL;
END;
$function$;

CREATE CONSTRAINT TRIGGER runner_placement_semantic_frontier_is_required
AFTER INSERT ON semantic_transcript_entry
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION require_runner_placement_semantic_frontier();
