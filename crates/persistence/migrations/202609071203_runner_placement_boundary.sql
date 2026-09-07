ALTER TABLE semantic_transcript_entry
    ADD COLUMN runner_placement_revision numeric(20,0),
    ADD CONSTRAINT semantic_runner_placement_reference_shape CHECK (
        (payload_kind = 'runner_placement_changed' AND runner_placement_revision BETWEEN 2 AND 18446744073709551615)
        OR (payload_kind <> 'runner_placement_changed' AND runner_placement_revision IS NULL)
    );

DO $$
DECLARE
    definition text;
    null_payload text;
BEGIN
    SELECT pg_get_constraintdef(oid) INTO STRICT definition FROM pg_constraint
        WHERE conrelid = 'semantic_transcript_entry'::regclass
          AND conname = 'semantic_transcript_entry_payload_kind_closed';
    ALTER TABLE semantic_transcript_entry DROP CONSTRAINT semantic_transcript_entry_payload_kind_closed;
    EXECUTE 'ALTER TABLE semantic_transcript_entry ADD CONSTRAINT semantic_transcript_entry_payload_kind_closed CHECK ('
        || substring(definition FROM 7) || ' OR payload_kind = ''runner_placement_changed'')';
    SELECT pg_get_constraintdef(oid) INTO STRICT definition FROM pg_constraint
        WHERE conrelid = 'semantic_transcript_entry'::regclass
          AND conname = 'semantic_transcript_entry_payload_shape';
    SELECT string_agg(format('%I IS NULL', attname), ' AND ' ORDER BY attnum) INTO null_payload
        FROM pg_attribute WHERE attrelid = 'semantic_transcript_entry'::regclass
          AND attnum > 0 AND NOT attisdropped
          AND attname NOT IN ('source_session_id', 'semantic_entry_id', 'payload_kind', 'runner_placement_revision');
    ALTER TABLE semantic_transcript_entry DROP CONSTRAINT semantic_transcript_entry_payload_shape;
    EXECUTE 'ALTER TABLE semantic_transcript_entry ADD CONSTRAINT semantic_transcript_entry_payload_shape CHECK ('
        || substring(definition FROM 7) || ' OR (payload_kind = ''runner_placement_changed'' AND ' || null_payload || '))';
END;
$$;

CREATE TABLE runner_placement_boundary (
    session_id uuid NOT NULL,
    placement_revision numeric(20,0) NOT NULL,
    event_ordinal numeric(20,0) NOT NULL,
    command_id uuid NOT NULL UNIQUE REFERENCES replace_lost_runner_result(command_id) DEFERRABLE INITIALLY DEFERRED,
    semantic_entry_id uuid NOT NULL,
    context_frontier_id uuid NOT NULL,
    PRIMARY KEY (session_id, placement_revision),
    UNIQUE (session_id, semantic_entry_id),
    FOREIGN KEY (session_id, event_ordinal) REFERENCES runner_session_placement_record(session_id, event_ordinal),
    FOREIGN KEY (session_id, semantic_entry_id) REFERENCES semantic_transcript_entry(source_session_id, semantic_entry_id) DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (session_id, context_frontier_id) REFERENCES context_frontier(owning_session_id, context_frontier_id) DEFERRABLE INITIALLY DEFERRED
);

ALTER TABLE semantic_transcript_entry ADD CONSTRAINT semantic_runner_placement_reference
    FOREIGN KEY (source_session_id, runner_placement_revision)
    REFERENCES runner_placement_boundary(session_id, placement_revision) DEFERRABLE INITIALLY DEFERRED;

CREATE TRIGGER runner_placement_boundary_is_append_only
    BEFORE UPDATE OR DELETE ON runner_placement_boundary
    FOR EACH ROW EXECUTE FUNCTION reject_immutable_record_change();

CREATE TABLE runner_session_placement_frontier (
    session_id uuid PRIMARY KEY REFERENCES session(session_id),
    placement_revision numeric(20,0) NOT NULL,
    FOREIGN KEY (session_id, placement_revision) REFERENCES runner_placement_boundary(session_id, placement_revision)
);

CREATE FUNCTION require_runner_placement_boundary() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    record runner_session_placement_record;
    entry semantic_transcript_entry;
    last_member record;
    prior_frontier uuid;
    prior_count bigint;
    longest_prior bigint := 0;
    actual_count bigint;
BEGIN
    SELECT * INTO STRICT record FROM runner_session_placement_record
        WHERE session_id = NEW.session_id AND event_ordinal = NEW.event_ordinal;
    SELECT * INTO STRICT entry FROM semantic_transcript_entry
        WHERE source_session_id = NEW.session_id AND semantic_entry_id = NEW.semantic_entry_id;
    IF record.event_kind <> 'runner_replaced' OR record.placement_revision <> NEW.placement_revision
       OR entry.payload_kind <> 'runner_placement_changed' OR entry.runner_placement_revision <> NEW.placement_revision THEN
        RAISE EXCEPTION 'placement boundary must reference its exact successor record' USING ERRCODE = '23514';
    END IF;
    IF EXISTS (SELECT 1 FROM model_call WHERE session_id = NEW.session_id
        AND state_kind IN ('in_flight', 'cancellation_requested')) THEN
        RAISE EXCEPTION 'placement boundary precedes a model observation' USING ERRCODE = '23514';
    END IF;
    FOR prior_frontier IN
        (SELECT turn_lifecycle_effective_terminal_frontier(session_id, turn_id)
            FROM turn_lifecycle WHERE session_id = NEW.session_id AND state_kind = 'terminal'
              AND terminal_frontier_id IS NOT NULL ORDER BY acceptance_position DESC LIMIT 1)
        UNION (SELECT context_frontier_id FROM runner_placement_boundary
            WHERE session_id = NEW.session_id AND placement_revision < NEW.placement_revision
            ORDER BY placement_revision DESC LIMIT 1)
        UNION SELECT compaction.result_frontier_id FROM context_compaction AS compaction
            WHERE compaction.session_id = NEW.session_id AND NOT EXISTS (SELECT 1 FROM context_compaction AS successor
                WHERE successor.predecessor_compaction_id = compaction.context_compaction_id)
    LOOP
        SELECT count(*) INTO prior_count FROM resolve_context_frontier_members(NEW.session_id, prior_frontier);
        longest_prior := greatest(longest_prior, prior_count);
        IF EXISTS (SELECT 1 FROM resolve_context_frontier_members(NEW.session_id, prior_frontier) AS prior
            LEFT JOIN resolve_context_frontier_members(NEW.session_id, NEW.context_frontier_id) AS next
                USING (member_position)
            WHERE prior.source_session_id IS DISTINCT FROM next.source_session_id
               OR prior.semantic_entry_id IS DISTINCT FROM next.semantic_entry_id) THEN
            RAISE EXCEPTION 'placement boundary must extend the authoritative prefix' USING ERRCODE = '23514';
        END IF;
    END LOOP;
    SELECT count(*) INTO actual_count FROM resolve_context_frontier_members(NEW.session_id, NEW.context_frontier_id);
    IF actual_count <> longest_prior + 1 THEN
        RAISE EXCEPTION 'placement boundary appends exactly one entry' USING ERRCODE = '23514';
    END IF;
    SELECT * INTO last_member FROM resolve_context_frontier_members(NEW.session_id, NEW.context_frontier_id)
        ORDER BY member_position DESC LIMIT 1;
    IF last_member.source_session_id IS DISTINCT FROM NEW.session_id
       OR last_member.semantic_entry_id IS DISTINCT FROM NEW.semantic_entry_id THEN
        RAISE EXCEPTION 'placement boundary must end its frontier' USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER runner_placement_boundary_is_complete
    AFTER INSERT ON runner_placement_boundary DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION require_runner_placement_boundary();

DO $$
DECLARE definition text;
BEGIN
    SELECT pg_get_functiondef('require_semantic_entry_turn_state()'::regprocedure) INTO definition;
    definition := regexp_replace(definition, 'BEGIN', $replacement$BEGIN
    IF NEW.payload_kind = 'runner_placement_changed' THEN
        IF NOT EXISTS (SELECT 1 FROM runner_placement_boundary
            WHERE session_id = NEW.source_session_id
              AND semantic_entry_id = NEW.semantic_entry_id
              AND placement_revision = NEW.runner_placement_revision) THEN
            RAISE EXCEPTION 'runner placement entry lacks its checked construction authority' USING ERRCODE = '23514';
        END IF;
        RETURN NULL;
    END IF;
$replacement$);
    EXECUTE definition;
END;
$$;
