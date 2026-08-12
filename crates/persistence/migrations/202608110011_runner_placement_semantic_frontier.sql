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
                      FROM semantic_transcript_entry AS prior_entry
                     WHERE prior_entry.source_session_id = pointer.session_id
                       AND prior_entry.semantic_entry_id <> entry.semantic_entry_id
                )
            )
            OR (
                prefix.context_frontier_id IS NOT NULL
                AND frontier.member_count = prefix.member_count + 1
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
