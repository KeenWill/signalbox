ALTER TABLE session_placement_event
    ALTER COLUMN provenance_command_id DROP NOT NULL,
    ADD COLUMN provenance_tool_request_id uuid,
    ADD COLUMN parent_placement_version numeric(20,0),
    ADD CONSTRAINT session_placement_provenance_shape CHECK (
        (provenance_command_id IS NOT NULL AND provenance_tool_request_id IS NULL AND parent_placement_version IS NULL)
        OR (provenance_command_id IS NULL AND provenance_tool_request_id IS NOT NULL
            AND event_kind = 'created' AND parent_placement_version IS NOT NULL
            AND parent_placement_version BETWEEN 1 AND 18446744073709551615)
    ),
    ADD CONSTRAINT session_placement_spawn_relationship FOREIGN KEY (provenance_tool_request_id, session_id)
        REFERENCES session_delegation(spawning_tool_request_id, child_session_id)
        DEFERRABLE INITIALLY DEFERRED;

CREATE FUNCTION require_delegated_creation_placement() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.provenance_tool_request_id IS NOT NULL AND NOT EXISTS (
        SELECT 1
          FROM session_delegation AS relation
          JOIN session_placement_event AS parent
            ON parent.session_id = relation.parent_session_id
           AND parent.version = NEW.parent_placement_version
         WHERE relation.spawning_tool_request_id = NEW.provenance_tool_request_id
           AND relation.child_session_id = NEW.session_id
           AND NEW.placement_path IS NOT DISTINCT FROM
               CASE WHEN parent.placement_path IS NULL THEN NULL
                    WHEN position('.' in parent.placement_path) = 0 THEN parent.placement_path
                    ELSE regexp_replace(parent.placement_path, '[^.]+$', '') || replace(NEW.session_id::text, '-', '') END
           AND NEW.root_global_read_intent = parent.root_global_read_intent
    ) THEN
        RAISE EXCEPTION 'delegated placement requires its parent directory proof'
            USING ERRCODE = '23514', CONSTRAINT = 'delegated_creation_placement';
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER delegated_creation_placement
    AFTER INSERT ON session_placement_event DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION require_delegated_creation_placement();
